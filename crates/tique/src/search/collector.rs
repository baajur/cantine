use tantivy::{
    collector::{Collector, SegmentCollector},
    fastfield::BytesFastFieldReader,
    schema::Field,
    Result, SegmentReader,
};

use crate::search::{AggregationRequest, FeatureValue, FeatureVector};

#[derive(Debug)]
pub struct FeatureRanges(Vec<Option<RangeVec>>);

impl FeatureRanges {
    fn merge(&mut self, other: &FeatureRanges) -> Result<()> {
        let FeatureRanges(inner) = self;

        if inner.len() != other.len() {
            return Err(tantivy::Error::SystemError(
                "Cannot merge FeatureRanges of different sizes".to_owned(),
            ));
        }

        for i in 0..inner.len() {
            // For every Some() RangeVec in the other
            if let Some(other_rv) = &other.get(i) {
                inner
                    .get_mut(i)
                    .expect("Bound by self.len(), should never happen")
                    .get_or_insert_with(|| RangeVec::new(other_rv.len()))
                    .merge(other_rv)?;
            }
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn new(size: usize) -> Self {
        assert!(size != 0);
        FeatureRanges(vec![None; size])
    }

    pub fn get(&self, idx: usize) -> &Option<RangeVec> {
        assert!(idx < self.len());
        &self.0[idx]
    }

    fn get_mut(&mut self, idx: usize) -> Option<&mut Option<RangeVec>> {
        self.0.get_mut(idx)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RangeVec(Vec<FeatureValue>);

impl RangeVec {
    fn new(size: usize) -> Self {
        assert!(size != 0);
        RangeVec(vec![0; size])
    }

    fn merge(&mut self, other: &RangeVec) -> Result<()> {
        let RangeVec(storage) = self;
        if storage.len() == other.len() {
            for i in 0..storage.len() {
                storage[i] += other.get(i);
            }

            Ok(())
        } else {
            Err(tantivy::TantivyError::SystemError(
                "Tried to merge RangeVec of different sizes".to_owned(),
            ))
        }
    }

    fn len(&self) -> usize {
        let RangeVec(storage) = self;
        storage.len()
    }

    fn get(&self, idx: usize) -> FeatureValue {
        assert!(idx < self.len());
        let RangeVec(storage) = self;
        storage[idx]
    }

    fn inc(&mut self, idx: usize) {
        assert!(idx < self.len());
        let RangeVec(storage) = self;
        storage[idx] += 1;
    }

    pub fn inner(&self) -> &Vec<FeatureValue> {
        &self.0
    }
}

pub struct FeatureCollector {
    field: Field,
    agg: FeatureRanges,
    wanted: AggregationRequest,
}

pub struct FeatureSegmentCollector {
    agg: FeatureRanges,
    reader: BytesFastFieldReader,
    wanted: AggregationRequest,
}

impl FeatureCollector {
    pub fn for_field(
        field: Field,
        num_features: usize,
        wanted: &AggregationRequest,
    ) -> FeatureCollector {
        FeatureCollector {
            field,
            wanted: wanted.clone(),
            agg: FeatureRanges::new(num_features),
        }
    }
}

impl Collector for FeatureCollector {
    type Fruit = FeatureRanges;
    type Child = FeatureSegmentCollector;

    fn for_segment(
        &self,
        _segment_local_id: u32,
        segment_reader: &SegmentReader,
    ) -> Result<FeatureSegmentCollector> {
        Ok(FeatureSegmentCollector {
            agg: FeatureRanges::new(self.agg.len()),
            wanted: self.wanted.clone(),
            reader: segment_reader
                .fast_fields()
                .bytes(self.field)
                .expect("Field is not a bytes fast field."),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, children: Vec<FeatureRanges>) -> Result<Self::Fruit> {
        let mut merged = FeatureRanges::new(self.agg.len());

        merged.merge(&self.agg)?;

        for child in children {
            merged.merge(&child)?;
        }

        Ok(merged)
    }
}

impl SegmentCollector for FeatureSegmentCollector {
    type Fruit = FeatureRanges;

    fn collect(&mut self, doc: u32, _score: f32) {
        let data = self.reader.get_bytes(doc);
        let doc_features = FeatureVector::parse(self.agg.len(), data).unwrap();

        for (feat, ranges) in &self.wanted {
            let opt = doc_features.get(*feat);

            // Document doesn't have this feature: Nothing to do
            if opt.is_none() {
                continue;
            }

            let value = opt.unwrap();

            // Wanted contains a feature that goes beyond num_features
            if *feat as usize > self.agg.len() {
                // XXX Add visibility to when this happens
                continue;
            }

            // Index/Count ranges in the order they were requested
            for (idx, range) in ranges.iter().enumerate() {
                if range.contains(&value) {
                    self.agg
                        .get_mut(*feat as usize)
                        // Guaranteed by the len() check above
                        .unwrap()
                        .get_or_insert_with(|| RangeVec::new(ranges.len()))
                        .inc(idx);
                }
            }
        }
    }

    fn harvest(self) -> <Self as SegmentCollector>::Fruit {
        self.agg
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    use tantivy::{
        self,
        query::AllQuery,
        schema::{Document, SchemaBuilder},
        Index,
    };

    const A: usize = 0;
    const B: usize = 1;
    const C: usize = 2;
    const D: usize = 3;

    #[test]
    fn cannot_merge_different_sized_range_vecs() {
        let mut ra = RangeVec::new(1);
        assert!(ra.merge(&RangeVec::new(2)).is_err());
    }

    #[test]
    fn range_vec_basic_usage() {
        let mut ra = RangeVec::new(1);
        assert_eq!(1, ra.len());

        assert_eq!(0, ra.get(0));
        ra.inc(0);
        assert_eq!(1, ra.get(0));
        ra.inc(0);
        assert_eq!(2, ra.get(0));
    }

    #[test]
    fn can_wrap_existing_vec() {
        let ra = RangeVec(vec![1, 0, 3]);
        assert_eq!(1, ra.get(0));
        assert_eq!(0, ra.get(1));
        assert_eq!(3, ra.get(2));
    }

    #[test]
    fn range_vec_merge() -> Result<()> {
        let mut ra = RangeVec::new(2);

        // Merging with a fresh one shouldn't change counts
        ra.merge(&RangeVec::new(2))?;
        assert_eq!(0, ra.get(0));
        assert_eq!(0, ra.get(1));

        // Zeroed ra: count should update to be the same as its src
        ra.merge(&RangeVec(vec![3, 0]))?;
        assert_eq!(3, ra.get(0));
        assert_eq!(0, ra.get(1));

        // And everything should increase properly
        ra.merge(&RangeVec(vec![417, 710]))?;
        assert_eq!(420, ra.get(0));
        assert_eq!(710, ra.get(1));

        Ok(())
    }

    #[test]
    fn feature_ranges_init() {
        let frs = FeatureRanges::new(2);
        assert_eq!(2, frs.len());

        assert_eq!(&None, frs.get(0));
        assert_eq!(&None, frs.get(1));
    }

    #[test]
    fn cannot_merge_different_sized_feature_ranges() {
        let mut a = FeatureRanges::new(1);
        assert!(a.merge(&FeatureRanges::new(2)).is_err());
    }

    #[test]
    fn cannot_merge_feature_ranges_with_uneven_ranges() {
        let mut a = FeatureRanges(vec![Some(RangeVec(vec![1]))]);
        let b = FeatureRanges(vec![Some(RangeVec(vec![1, 2]))]);
        assert_eq!(a.len(), b.len());
        // a.len() == b.len(), but the inner ranges aren't even
        assert!(a.merge(&b).is_err());
    }

    #[test]
    fn feature_ranges_merge() -> Result<()> {
        let mut a = FeatureRanges::new(2);

        // Merge with empty: nothing changes
        a.merge(&FeatureRanges::new(2))?;
        assert_eq!(&None, a.get(0));
        assert_eq!(&None, a.get(1));

        // Empty merged with filled: copy
        {
            let src = FeatureRanges(vec![Some(RangeVec(vec![1])), Some(RangeVec(vec![2, 3]))]);
            a.merge(&src)?;

            assert_eq!(&Some(RangeVec(vec![1])), a.get(0));
            assert_eq!(&Some(RangeVec(vec![2, 3])), a.get(1));
        }

        // Non empty: just update ranges
        {
            let src = FeatureRanges(vec![Some(RangeVec(vec![41])), Some(RangeVec(vec![0, 4]))]);
            a.merge(&src)?;

            assert_eq!(&Some(RangeVec(vec![42])), a.get(0));
            assert_eq!(&Some(RangeVec(vec![2, 7])), a.get(1));
        }

        Ok(())
    }

    #[test]
    fn usage() -> Result<()> {
        // First we create a basic index where there schema is just a bytes field
        let mut sb = SchemaBuilder::new();
        let field = sb.add_bytes_field("bytes");
        let schema = sb.build();

        let index = Index::create_in_ram(schema);
        let mut writer = index.writer_with_num_threads(1, 40_000_000)?;

        let add_doc = |fv: FeatureVector<&mut [u8], usize>| -> Result<()> {
            let mut doc = Document::default();
            doc.add_bytes(field, fv.as_bytes().to_owned());
            writer.add_document(doc);
            Ok(())
        };

        // And we populate it with a couple of docs where
        // the bytes field is a features::FeatureVector
        let num_features = 4;
        let empty_buffer = vec![std::u8::MAX; 4 * 2];

        {
            // Doc{ A: 5, B: 10}
            let mut buf = empty_buffer.clone();
            let mut fv = FeatureVector::parse(num_features, buf.as_mut_slice()).unwrap();
            fv.set(A, 5).unwrap();
            fv.set(B, 10).unwrap();
            add_doc(fv)?;
        }

        {
            // Doc{ A: 7, C: 2}
            let mut buf = empty_buffer.clone();
            let mut fv = FeatureVector::parse(num_features, buf.as_mut_slice()).unwrap();
            fv.set(A, 7).unwrap();
            fv.set(C, 2).unwrap();
            add_doc(fv)?;
        }

        writer.commit()?;

        let reader = index.reader()?;
        let searcher = reader.searcher();

        let wanted: AggregationRequest = vec![
            // feature A between ranges 2-10 and 0-5
            (A, vec![2..=10, 0..=5]),
            // and so on...
            (B, vec![9..=100, 420..=710]),
            (C, vec![2..=2]),
            (D, vec![]),
        ];

        let feature_ranges = searcher.search(
            &AllQuery,
            &FeatureCollector::for_field(field, num_features, &wanted),
        )?;

        // { A => { "2-10": 2, "0-5": 1 } }
        assert_eq!(&Some(RangeVec(vec![2, 1])), feature_ranges.get(A as usize));
        // { B => { "9-100": 1, "420-710": 0 } }
        assert_eq!(&Some(RangeVec(vec![1, 0])), feature_ranges.get(B as usize));
        // { C => { "2" => 1 } }
        assert_eq!(&Some(RangeVec(vec![1])), feature_ranges.get(C as usize));
        // Asking to count a feature but providing no ranges should no-op
        assert_eq!(&None, feature_ranges.get(D as usize));

        Ok(())
    }
}

use std::ops::{AddAssign, RangeInclusive};

use tantivy::{
    collector::{Collector, SegmentCollector},
    fastfield::BytesFastFieldReader,
    schema::Field,
    Result, SegmentReader,
};

use super::FeatureVector;

pub type AggregationRequest<T> = Vec<(usize, Vec<RangeInclusive<T>>)>;
pub type FeatureRanges<T> = Vec<Option<Vec<T>>>;

fn merge_feature_ranges<'a, T>(
    dest: &'a mut FeatureRanges<T>,
    src: &'a [Option<Vec<T>>],
) -> Result<()>
where
    T: AddAssign<&'a T> + Clone,
{
    if dest.len() == src.len() {
        // All I'm doing here is summing a sparse x dense matrix. Rice?
        for (i, mine) in dest.iter_mut().enumerate() {
            if let Some(ranges) = &src[i] {
                if let Some(current) = mine {
                    merge_ranges(current, &ranges)?;
                } else {
                    mine.replace(ranges.clone());
                }
            }
        }
        Ok(())
    } else {
        Err(tantivy::TantivyError::SystemError(
            "Tried to merge uneven feature ranges".to_owned(),
        ))
    }
}

fn merge_ranges<'a, T>(dest: &'a mut [T], src: &'a [T]) -> Result<()>
where
    T: AddAssign<&'a T>,
{
    if dest.len() == src.len() {
        for (i, src_item) in src.iter().enumerate() {
            dest[i] += src_item;
        }
        Ok(())
    } else {
        Err(tantivy::TantivyError::SystemError(
            "Tried to merge uneven range vecs".to_owned(),
        ))
    }
}

pub struct FeatureCollector<T> {
    field: Field,
    agg: FeatureRanges<T>,
    wanted: AggregationRequest<T>,
    unset_value: Option<T>,
}

pub struct FeatureSegmentCollector<T> {
    // do I need agg here?
    agg: FeatureRanges<T>,
    reader: BytesFastFieldReader,
    wanted: AggregationRequest<T>,
    unset_value: Option<T>,
}

impl<T> FeatureCollector<T>
where
    for<'a> T: Copy + AddAssign<&'a T>,
{
    pub fn for_field(
        field: Field,
        num_features: usize,
        unset_value: Option<T>,
        wanted: &[(usize, Vec<RangeInclusive<T>>)],
    ) -> FeatureCollector<T> {
        FeatureCollector {
            field,
            wanted: wanted.to_vec(),
            agg: vec![None; num_features],
            unset_value,
        }
    }
}

macro_rules! collector_impl {
    ($t: ty) => {
        impl Collector for FeatureCollector<$t> {
            type Fruit = FeatureRanges<$t>;
            type Child = FeatureSegmentCollector<$t>;

            fn for_segment(
                &self,
                _segment_local_id: u32,
                segment_reader: &SegmentReader,
            ) -> Result<Self::Child> {
                Ok(FeatureSegmentCollector {
                    agg: vec![None; self.agg.len()],
                    wanted: self.wanted.clone(),
                    reader: segment_reader
                        .fast_fields()
                        .bytes(self.field)
                        .expect("Field is not a bytes fast field."),
                    unset_value: self.unset_value,
                })
            }

            fn requires_scoring(&self) -> bool {
                false
            }

            fn merge_fruits(&self, children: Vec<Self::Fruit>) -> Result<Self::Fruit> {
                let mut merged = vec![None; self.agg.len()];
                merge_feature_ranges(&mut merged, &self.agg)?;

                for child in children {
                    merge_feature_ranges(&mut merged, &child)?;
                }

                Ok(merged)
            }
        }

        impl SegmentCollector for FeatureSegmentCollector<$t> {
            type Fruit = FeatureRanges<$t>;

            fn collect(&mut self, doc: u32, _score: f32) {
                let data = self.reader.get_bytes(doc);
                let doc_features =
                    FeatureVector::<_, $t>::parse(data, self.agg.len(), self.unset_value).unwrap();

                for (feat, ranges) in &self.wanted {
                    // Wanted contains a feature that goes beyond num_features
                    if *feat > self.agg.len() {
                        // XXX Add visibility to when this happens
                        continue;
                    }

                    let opt = doc_features.get(*feat);

                    // Document doesn't have this feature: Nothing to do
                    if opt.is_none() {
                        continue;
                    }

                    let value = opt.unwrap();

                    // Index/Count ranges in the order they were requested
                    for (idx, range) in ranges.iter().enumerate() {
                        if range.contains(&value) {
                            self.agg
                                .get_mut(*feat)
                                .expect("agg should have been initialized by now")
                                .get_or_insert_with(|| vec![0; ranges.len()])[idx] += 1;
                        }
                    }
                }
            }

            fn harvest(self) -> <Self as SegmentCollector>::Fruit {
                self.agg
            }
        }
    };
}

collector_impl!(u16);
collector_impl!(u32);
collector_impl!(u64);
collector_impl!(i16);
collector_impl!(i32);
collector_impl!(i64);

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
    fn cannot_merge_uneven_rangevec() {
        assert!(merge_ranges(&mut [0u16], &[1, 2]).is_err());
    }

    #[test]
    fn cannot_merge_unever_feature_ranges() {
        assert!(merge_feature_ranges::<u16>(&mut vec![None], &[None, None]).is_err());
    }

    #[test]
    fn range_vec_merge() -> Result<()> {
        let mut ra = vec![0u16, 0];
        // Merging with a fresh one shouldn't change counts
        merge_ranges(&mut ra, &[0, 0])?;
        assert_eq!(0, ra[0]);
        assert_eq!(0, ra[1]);

        // Zeroed ra: count should update to be the same as its src
        merge_ranges(&mut ra, &[3, 0])?;
        assert_eq!(3, ra[0]);
        assert_eq!(0, ra[1]);

        // And everything should increase properly
        merge_ranges(&mut ra, &[417, 710])?;
        assert_eq!(420, ra[0]);
        assert_eq!(710, ra[1]);

        Ok(())
    }

    #[test]
    fn feature_ranges_merge() -> Result<()> {
        let mut a: FeatureRanges<u16> = vec![None, None];

        merge_feature_ranges(&mut a, &[None, None])?;
        assert_eq!(None, a[0]);
        assert_eq!(None, a[1]);

        // Empty merged with filled: copy
        {
            let src = vec![Some(vec![1]), Some(vec![2, 3])];
            merge_feature_ranges(&mut a, &src)?;

            assert_eq!(Some(vec![1]), a[0]);
            assert_eq!(Some(vec![2, 3]), a[1]);
        }

        // Non empty: just update ranges
        {
            let src = vec![Some(vec![41]), Some(vec![0, 4])];
            merge_feature_ranges(&mut a, &src)?;

            assert_eq!(Some(vec![42]), a[0]);
            assert_eq!(Some(vec![2, 7]), a[1]);
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

        let add_doc = |fv: FeatureVector<&mut [u8], u16>| -> Result<()> {
            let mut doc = Document::default();
            doc.add_bytes(field, fv.as_bytes().to_owned());
            writer.add_document(doc);
            Ok(())
        };

        // And we populate it with a couple of docs where
        // the bytes field is a features::FeatureVector
        let num_features = 4;
        let empty_buffer = vec![std::u8::MAX; num_features * 2];
        let unset = Some(std::u16::MAX);

        {
            // Doc{ A: 5, B: 10}
            let mut buf = empty_buffer.clone();
            let mut fv =
                FeatureVector::<_, u16>::parse(buf.as_mut_slice(), num_features, unset).unwrap();
            fv.set(A, 5).unwrap();
            fv.set(B, 10).unwrap();
            add_doc(fv)?;
        }

        {
            // Doc{ A: 7, C: 2}
            let mut buf = empty_buffer.clone();
            let mut fv =
                FeatureVector::<_, u16>::parse(buf.as_mut_slice(), num_features, unset).unwrap();
            fv.set(A, 7).unwrap();
            fv.set(C, 2).unwrap();
            add_doc(fv)?;
        }

        writer.commit()?;

        let reader = index.reader()?;
        let searcher = reader.searcher();

        let wanted: AggregationRequest<u16> = vec![
            // feature A between ranges 2-10 and 0-5
            (A, vec![2..=10, 0..=5]),
            // and so on...
            (B, vec![9..=100, 420..=710]),
            (C, vec![2..=2]),
            (D, vec![]),
        ];

        let feature_ranges = searcher.search(
            &AllQuery,
            &FeatureCollector::for_field(field, num_features, unset, &wanted),
        )?;

        // { A => { "2-10": 2, "0-5": 1 } }
        assert_eq!(Some(vec![2u16, 1]), feature_ranges[A]);
        // { B => { "9-100": 1, "420-710": 0 } }
        assert_eq!(Some(vec![1, 0]), feature_ranges[B]);
        // { C => { "2" => 1 } }
        assert_eq!(Some(vec![1]), feature_ranges[C]);
        // Asking to count a feature but providing no ranges should no-op
        assert_eq!(None, feature_ranges[D]);

        Ok(())
    }
}
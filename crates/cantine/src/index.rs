use std::{cmp::Ordering, convert::TryFrom};

use bincode;
use serde::{Deserialize, Serialize};
use tantivy::{
    self,
    query::Query,
    schema::{Field, Schema, SchemaBuilder, Value, FAST, STORED, TEXT},
    Document, Result, Searcher, SegmentReader, TantivyError,
};

use crate::model::{
    FeaturesAggregationQuery, FeaturesAggregationResult, FeaturesCollector, FeaturesFilterFields,
    Recipe, RecipeId, Sort,
};

use tique::top_collector::{
    ordered_by_f64_fast_field, ordered_by_u64_fast_field, ConditionalTopCollector, SearchMarker,
};

#[derive(Clone)]
pub struct RecipeIndex {
    pub id: Field,
    pub fulltext: Field,
    pub features_bincode: Field,
    pub features: FeaturesFilterFields,
}

const FIELD_ID: &str = "id";
const FIELD_FULLTEXT: &str = "fulltext";
const FIELD_FEATURES_BINCODE: &str = "features_bincode";

impl RecipeIndex {
    pub fn make_document(&self, recipe: &Recipe) -> Document {
        let mut doc = Document::new();
        doc.add_u64(self.id, recipe.recipe_id);

        let mut fulltext = Vec::new();

        fulltext.push(recipe.name.as_str());
        for ingredient in &recipe.ingredients {
            fulltext.push(ingredient.as_str());
        }
        for instruction in &recipe.instructions {
            fulltext.push(instruction.as_str());
        }
        doc.add_text(self.fulltext, fulltext.join("\n").as_str());

        doc.add_bytes(
            self.features_bincode,
            bincode::serialize(&recipe.features).unwrap(),
        );

        self.features.add_to_doc(&mut doc, &recipe.features);
        doc
    }

    fn addresses_to_ids<T>(
        &self,
        searcher: &Searcher,
        addresses: &[SearchMarker<T>],
    ) -> Result<Vec<RecipeId>> {
        let mut items = Vec::with_capacity(addresses.len());

        for addr in addresses.iter() {
            let doc = searcher.doc(addr.doc)?;
            if let Some(&Value::U64(id)) = doc.get_first(self.id) {
                items.push(id);
            } else {
                panic!("Found document without a stored id");
            }
        }

        Ok(items)
    }

    pub fn search(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        limit: usize,
        sort: Sort,
        after: After,
    ) -> Result<(usize, Vec<RecipeId>, Option<After>)> {
        macro_rules! condition_from_score {
            ($score:expr) => {{
                let after_score = $score;
                let after_id = after.recipe_id();
                let is_start = after.is_start();

                let id_field = self.id;
                move |reader: &SegmentReader| {
                    let id_reader = reader
                        .fast_fields()
                        .u64(id_field)
                        .expect("id field is indexed with the FAST flag");

                    move |_segment_id, doc_id, score| {
                        if is_start {
                            return true;
                        }

                        let recipe_id = id_reader.get(doc_id);
                        match after_score.partial_cmp(&score) {
                            Some(Ordering::Greater) => true,
                            Some(Ordering::Equal) => after_id < recipe_id,
                            _ => false,
                        }
                    }
                }
            }};
        }

        macro_rules! collect_unsigned {
            ($field:ident) => {{
                let condition = condition_from_score!(after.score());
                let top_collector =
                    ordered_by_u64_fast_field(self.features.$field, limit, condition);

                let result = searcher.search(query, &top_collector)?;
                let items = self.addresses_to_ids(&searcher, &result.items)?;

                let num_items = items.len();
                let cursor = if result.visited.saturating_sub(num_items) > 0 {
                    let last_score = result.items[num_items - 1].score;
                    let last_id = items[num_items - 1];
                    Some(After::new(last_score, last_id))
                } else {
                    None
                };

                Ok((result.total, items, cursor))
            }};
        }

        macro_rules! collect_float {
            ($field:ident) => {{
                let condition = condition_from_score!(after.score_f64());
                let top_collector =
                    ordered_by_f64_fast_field(self.features.$field, limit, condition);

                let result = searcher.search(query, &top_collector)?;
                let items = self.addresses_to_ids(&searcher, &result.items)?;

                let num_items = items.len();
                let cursor = if result.visited.saturating_sub(num_items) > 0 {
                    let last_score = result.items[num_items - 1].score;
                    let last_id = items[num_items - 1];
                    Some(After::from_f64(last_score, last_id))
                } else {
                    None
                };

                Ok((result.total, items, cursor))
            }};
        }

        match sort {
            Sort::Relevance => {
                let condition = condition_from_score!(after.score_f32());
                let top_collector = ConditionalTopCollector::with_limit(limit, condition);

                let result = searcher.search(query, &top_collector)?;
                let items = self.addresses_to_ids(&searcher, &result.items)?;

                let num_items = items.len();
                let cursor = if result.visited.saturating_sub(num_items) > 0 {
                    let last_score = result.items[num_items - 1].score;
                    let last_id = items[num_items - 1];
                    Some(After::from_f32(last_score, last_id))
                } else {
                    None
                };

                Ok((result.total, items, cursor))
            }
            Sort::NumIngredients => collect_unsigned!(num_ingredients),
            Sort::InstructionsLength => collect_unsigned!(instructions_length),
            Sort::TotalTime => collect_unsigned!(total_time),
            Sort::CookTime => collect_unsigned!(cook_time),
            Sort::PrepTime => collect_unsigned!(prep_time),
            Sort::Calories => collect_unsigned!(calories),
            Sort::FatContent => collect_float!(fat_content),
            Sort::CarbContent => collect_float!(carbohydrate_content),
            Sort::ProteinContent => collect_float!(protein_content),
        }
    }

    pub fn aggregate_features(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        agg_query: FeaturesAggregationQuery,
    ) -> Result<FeaturesAggregationResult> {
        let features_field = self.features_bincode;
        let collector = FeaturesCollector::new(agg_query, move |reader: &SegmentReader| {
            let features_reader = reader
                .fast_fields()
                .bytes(features_field)
                .expect("bytes field is indexed");

            move |doc, query, agg| {
                let buf = features_reader.get_bytes(doc);
                let features = bincode::deserialize(buf).unwrap();
                agg.collect(query, &features);
            }
        });

        Ok(searcher.search(query, &collector)?)
    }
}

impl From<&mut SchemaBuilder> for RecipeIndex {
    fn from(builder: &mut SchemaBuilder) -> Self {
        RecipeIndex {
            id: builder.add_u64_field(FIELD_ID, STORED | FAST),
            fulltext: builder.add_text_field(FIELD_FULLTEXT, TEXT),
            features_bincode: builder.add_bytes_field(FIELD_FEATURES_BINCODE),
            features: FeaturesFilterFields::from(builder),
        }
    }
}

impl TryFrom<&Schema> for RecipeIndex {
    type Error = TantivyError;

    fn try_from(schema: &Schema) -> Result<Self> {
        let id = schema
            .get_field(FIELD_ID)
            .ok_or_else(|| TantivyError::SchemaError(format!("Missing field {}", FIELD_ID)))?;

        let fulltext = schema.get_field(FIELD_FULLTEXT).ok_or_else(|| {
            TantivyError::SchemaError(format!("Missing field {}", FIELD_FULLTEXT))
        })?;

        let features_bincode = schema.get_field(FIELD_FEATURES_BINCODE).ok_or_else(|| {
            TantivyError::SchemaError(format!("Missing field {}", FIELD_FEATURES_BINCODE))
        })?;

        Ok(RecipeIndex {
            id,
            fulltext,
            features_bincode,
            features: FeaturesFilterFields::try_from(schema)?,
        })
    }
}

pub type RecipeIndexSearchResult = (
    usize,
    Vec<RecipeId>,
    Option<After>,
    Option<FeaturesAggregationResult>,
);

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct After(u64, RecipeId);

impl After {
    pub const START: Self = Self(0, 0);

    pub fn new(score: u64, recipe_id: RecipeId) -> Self {
        Self(score, recipe_id)
    }

    pub fn from_f32(score: f32, recipe_id: RecipeId) -> Self {
        Self(score.to_bits() as u64, recipe_id)
    }

    pub fn from_f64(score: f64, recipe_id: RecipeId) -> Self {
        Self(score.to_bits(), recipe_id)
    }

    pub fn is_start(&self) -> bool {
        self.0 == 0 && self.1 == 0
    }

    pub fn recipe_id(&self) -> RecipeId {
        self.1
    }

    pub fn score(&self) -> u64 {
        self.0
    }

    pub fn score_f32(&self) -> f32 {
        f32::from_bits(self.0 as u32)
    }

    pub fn score_f64(&self) -> f64 {
        f64::from_bits(self.0)
    }
}

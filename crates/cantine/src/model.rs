use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::database::DatabaseRecord;
use cantine_derive::FilterAndAggregation;

#[derive(Deserialize, Serialize, Debug, PartialEq)]
pub struct Recipe {
    pub uuid: Uuid,

    pub recipe_id: RecipeId,
    pub name: String,
    pub crawl_url: String,

    pub ingredients: Vec<String>,
    pub instructions: Vec<String>,
    pub images: Vec<String>,

    pub similar_recipe_ids: Vec<u64>,

    pub features: Features,
}

pub type RecipeId = u64;

impl DatabaseRecord for Recipe {
    fn get_id(&self) -> u64 {
        self.recipe_id
    }
    fn get_uuid(&self) -> &Uuid {
        &self.uuid
    }
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct RecipeCard {
    pub name: String,
    pub uuid: Uuid,
    pub crawl_url: String,
    pub num_ingredients: u8,
    pub instructions_length: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_time: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calories: Option<u32>,
}

impl From<Recipe> for RecipeCard {
    fn from(src: Recipe) -> Self {
        Self {
            name: src.name,
            uuid: src.uuid,
            crawl_url: src.crawl_url,
            num_ingredients: src.features.num_ingredients,
            instructions_length: src.features.instructions_length,
            total_time: src.features.total_time,
            calories: src.features.calories,
        }
    }
}

#[derive(FilterAndAggregation, Serialize, Deserialize, Debug, Default, PartialEq)]
pub struct Features {
    pub num_ingredients: u8,
    pub instructions_length: u32,

    pub prep_time: Option<u32>,
    pub total_time: Option<u32>,
    pub cook_time: Option<u32>,

    pub calories: Option<u32>,
    pub fat_content: Option<f32>,
    pub carbohydrate_content: Option<f32>,
    pub protein_content: Option<f32>,

    pub diet_lowcarb: Option<f32>,
    pub diet_vegetarian: Option<f32>,
    pub diet_vegan: Option<f32>,
    pub diet_keto: Option<f32>,
    pub diet_paleo: Option<f32>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    Relevance,
    NumIngredients,
    InstructionsLength,
    TotalTime,
    CookTime,
    PrepTime,
    Calories,
    FatContent,
    CarbContent,
    ProteinContent,
}

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct SearchQuery {
    pub fulltext: Option<String>,
    pub sort: Option<Sort>,
    pub num_items: Option<u8>,
    pub filter: Option<FeaturesFilterQuery>,
    pub agg: Option<FeaturesAggregationQuery>,
    pub after: Option<SearchCursor>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct SearchResult {
    pub items: Vec<RecipeCard>,
    pub total_found: usize,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub agg: Option<FeaturesAggregationResult>,

    // XXX Maybe wrap the cursor so that we translate uuid<->id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<SearchCursor>,
}

// FIXME Saner serialization
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct SearchCursor(u64, RecipeId);

impl SearchCursor {
    pub const START: Self = Self(0, 0);

    pub fn new(score: u64, recipe_id: RecipeId) -> Self {
        Self(score, recipe_id)
    }

    pub fn from_f32(score: f32, recipe_id: RecipeId) -> Self {
        Self(score.to_bits() as u64, recipe_id)
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
}

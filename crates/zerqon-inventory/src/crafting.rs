//! Pure 2×2 recipe matching and validated crafting remainder definitions.
//! Menu transactions consume ingredients and place outputs atomically.

use crate::{ItemCatalog, ItemError, ItemId, ItemStack, Slot, ValidatedPatch};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

const MAX_RECIPES: usize = 8192;
const MAX_TAGS: usize = 4096;
const MAX_VALUES: usize = 1_048_576;
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_ALTERNATIVES: usize = 65_536;

/// A preview for one craft, borrowing no mutable inventory state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CraftingMatch {
    pub recipe_id: String,
    pub output: ItemStack,
    /// Zero-based row-major grid indices, unique and ascending. Consume one item
    /// from each only as part of a separately validated inventory transaction.
    pub consumed_slots: Vec<usize>,
}

/// Why a recipe was deliberately excluded from this limited matcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CraftingExclusionReason {
    UnsupportedType(String),
    DoesNotFit,
    OutputComponents(Vec<String>),
    OutputItemHook,
    UnsupportedFields(Vec<String>),
}

/// An exclusion is observable instead of silently becoming a generic recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CraftingExclusion {
    pub recipe_id: String,
    pub reason: CraftingExclusionReason,
}

/// Loading decisions, both lists in lexicographic recipe ID order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CraftingReport {
    pub supported: Vec<String>,
    pub excluded: Vec<CraftingExclusion>,
}

/// Malformed data fails loading, with the originating recipe or tag ID.
#[derive(Debug, thiserror::Error)]
pub enum CraftingError {
    #[error(transparent)]
    Data(#[from] ItemError),
    #[error("invalid crafting recipe {recipe_id}: {source}")]
    Recipe {
        recipe_id: String,
        source: ItemError,
    },
    #[error("invalid crafting tag {tag_id}: {source}")]
    Tag { tag_id: String, source: ItemError },
}

type Ingredient = Vec<ItemId>;

#[derive(Debug)]
enum Pattern {
    Shaped {
        width: usize,
        height: usize,
        cells: Vec<Option<Ingredient>>,
    },
    Shapeless(Vec<Ingredient>),
}

#[derive(Debug)]
struct Recipe {
    pattern: Pattern,
    output: ItemStack,
}

/// A bounded, immutable recipe book. The caller must use the same item catalog
/// to compile recipes and validate grids. First-match priority is lexicographic
/// recipe ID order, a scoped policy rather than a claim about vanilla priority.
#[derive(Debug, Default)]
pub struct CraftingRecipes {
    recipes: BTreeMap<String, Recipe>,
    report: CraftingReport,
}

/// Validated per-item crafting remainders supplied by the original item facts.
/// This is unrelated to the consumption/use-remainder item component.
#[derive(Debug, Default)]
pub struct CraftingRemainders {
    entries: BTreeMap<ItemId, ItemStack>,
}

impl CraftingRemainders {
    pub fn from_definitions(
        catalog: &ItemCatalog,
        entries: BTreeMap<ItemId, ItemStack>,
    ) -> Result<Self, ItemError> {
        if entries.len() > catalog.item_count() {
            return Err(ItemError::Limit("crafting remainder entries"));
        }
        for (source, remainder) in &entries {
            if catalog.item(*source)?.key == "minecraft:air" {
                return Err(ItemError::Invalid("air cannot have a crafting remainder"));
            }
            catalog.validate_stack(remainder)?;
            if !catalog.supports_generic_click(remainder)? {
                return Err(ItemError::Invalid(
                    "unsupported crafting remainder item hook",
                ));
            }
        }
        Ok(Self { entries })
    }

    pub fn get(&self, item: ItemId) -> Option<&ItemStack> {
        self.entries.get(&item)
    }
}

impl CraftingRecipes {
    /// Compile raw recipe JSON and raw item-tag JSON keyed by namespaced IDs.
    /// Tags use `{"values":["item", "#tag", {"id":"item","required":false}]}`.
    /// Ordinary output component patches are deliberately excluded in this slice.
    /// Result counts must be positive JSON integers in 1..99, additionally bounded
    /// by the item stack limit. Java's floating-point truncation is deliberately
    /// rejected; absent or null optional counts use the original default of one.
    /// Size, nesting, and cumulative tag/ingredient expansion are bounded before
    /// allocating the corresponding internal data. Malformed ordinary data errors.
    pub fn from_json(
        catalog: &ItemCatalog,
        recipes: &BTreeMap<String, Value>,
        tags: &BTreeMap<String, Value>,
    ) -> Result<Self, CraftingError> {
        if recipes.len() > MAX_RECIPES || tags.len() > MAX_TAGS {
            return Err(ItemError::Limit("crafting recipe or tag count").into());
        }
        let mut budget = JsonBudget::default();
        for (id, value) in recipes.iter().chain(tags) {
            if !crate::catalog::valid_identifier(id) {
                return Err(ItemError::Invalid("invalid recipe or tag identifier").into());
            }
            budget.visit(value, 0)?;
        }
        let mut resolver = TagResolver {
            catalog,
            tags,
            resolved: BTreeMap::new(),
            visiting: BTreeSet::new(),
            expanded: 0,
        };
        for id in tags.keys() {
            resolver.resolve(id)?;
        }
        let mut book = Self::default();
        for (id, value) in recipes {
            match parse_recipe(value, &mut resolver).map_err(|source| CraftingError::Recipe {
                recipe_id: id.clone(),
                source,
            })? {
                Ok(recipe) => {
                    book.recipes.insert(id.clone(), recipe);
                    book.report.supported.push(id.clone());
                }
                Err(reason) => book.report.excluded.push(CraftingExclusion {
                    recipe_id: id.clone(),
                    reason,
                }),
            }
        }
        Ok(book)
    }

    pub fn report(&self) -> &CraftingReport {
        &self.report
    }

    /// Find a one-craft preview. Every present input is validated even if the
    /// recipe book is empty. Input item components do not affect ordinary
    /// ingredient identity and are never copied onto the output.
    pub fn find_match(
        &self,
        grid: &[Slot; 4],
        catalog: &ItemCatalog,
    ) -> Result<Option<CraftingMatch>, ItemError> {
        for stack in grid.iter().flatten() {
            catalog.validate_stack(stack)?;
        }
        for (id, recipe) in &self.recipes {
            if recipe.pattern.matches(grid) {
                catalog.validate_stack(&recipe.output)?;
                return Ok(Some(CraftingMatch {
                    recipe_id: id.clone(),
                    output: recipe.output.clone(),
                    consumed_slots: grid
                        .iter()
                        .enumerate()
                        .filter_map(|(index, slot)| slot.as_ref().map(|_| index))
                        .collect(),
                }));
            }
        }
        Ok(None)
    }
}

impl Pattern {
    fn matches(&self, grid: &[Slot; 4]) -> bool {
        match self {
            Self::Shaped {
                width,
                height,
                cells,
            } => {
                for offset_y in 0..=2 - height {
                    for offset_x in 0..=2 - width {
                        for mirror in [false, true] {
                            let matches = grid.iter().enumerate().all(|(index, slot)| {
                                let (x, y) = (index % 2, index / 2);
                                let ingredient = if x >= offset_x
                                    && y >= offset_y
                                    && x < offset_x + width
                                    && y < offset_y + height
                                {
                                    let local_x = x - offset_x;
                                    let local_x =
                                        if mirror { width - 1 - local_x } else { local_x };
                                    cells[(y - offset_y) * width + local_x].as_ref()
                                } else {
                                    None
                                };
                                match (ingredient, slot) {
                                    (None, None) => true,
                                    (Some(ingredient), Some(stack)) => {
                                        ingredient.binary_search(&stack.item()).is_ok()
                                    }
                                    _ => false,
                                }
                            });
                            if matches {
                                return true;
                            }
                        }
                    }
                }
                false
            }
            Self::Shapeless(ingredients) => {
                if grid.iter().flatten().count() != ingredients.len() {
                    return false;
                }
                assign_ingredients(ingredients, grid, 0)
            }
        }
    }
}

// At most four ingredients: exhaustive assignment has at most 4! leaves and
// avoids a greedy choice stealing the only valid slot from another ingredient.
fn assign_ingredients(ingredients: &[Ingredient], grid: &[Slot; 4], used: u8) -> bool {
    let Some((first, remaining)) = ingredients.split_first() else {
        return true;
    };
    grid.iter().enumerate().any(|(index, slot)| {
        used & (1 << index) == 0
            && slot
                .as_ref()
                .is_some_and(|stack| first.binary_search(&stack.item()).is_ok())
            && assign_ingredients(remaining, grid, used | (1 << index))
    })
}

fn parse_recipe(
    value: &Value,
    resolver: &mut TagResolver<'_>,
) -> Result<Result<Recipe, CraftingExclusionReason>, ItemError> {
    let object = object(value)?;
    let recipe_type = identifier(string(object.get("type"))?)?;
    let shaped = match recipe_type.as_str() {
        "minecraft:crafting_shaped" => true,
        "minecraft:crafting_shapeless" => false,
        _ => return Ok(Err(CraftingExclusionReason::UnsupportedType(recipe_type))),
    };
    validate_metadata(object)?;
    let allowed = if shaped {
        &[
            "type",
            "group",
            "category",
            "key",
            "pattern",
            "result",
            "show_notification",
        ][..]
    } else {
        &[
            "type",
            "group",
            "category",
            "ingredients",
            "result",
            "show_notification",
        ][..]
    };
    let unknown: Vec<_> = object
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Ok(Err(CraftingExclusionReason::UnsupportedFields(unknown)));
    }
    let pattern = if shaped {
        parse_shaped(object, resolver)?
    } else {
        let values = object
            .get("ingredients")
            .and_then(Value::as_array)
            .ok_or(ItemError::Invalid("missing shapeless ingredients"))?;
        if values.is_empty() || values.len() > 9 {
            return Err(ItemError::Invalid(
                "shapeless ingredient count must be 1..9",
            ));
        }
        if values.len() > 4 {
            return Ok(Err(CraftingExclusionReason::DoesNotFit));
        }
        Ok(Pattern::Shapeless(
            values
                .iter()
                .map(|value| resolver.ingredient(value))
                .collect::<Result<_, _>>()?,
        ))
    };
    let pattern = match pattern {
        Ok(pattern) => pattern,
        Err(reason) => return Ok(Err(reason)),
    };
    let result = object
        .get("result")
        .ok_or(ItemError::Invalid("missing crafting result"))?;
    let (output_id, count) = if let Some(name) = result.as_str() {
        (resolver.item(name)?, 1)
    } else {
        let result = self::object(result)?;
        let unknown: Vec<_> = result
            .keys()
            .filter(|key| !["id", "count", "components"].contains(&key.as_str()))
            .map(|key| format!("result.{key}"))
            .collect();
        if !unknown.is_empty() {
            return Ok(Err(CraftingExclusionReason::UnsupportedFields(unknown)));
        }
        if let Some(components) = result.get("components").filter(|value| !value.is_null()) {
            let components = self::object(components)?;
            if !components.is_empty() {
                return Ok(Err(CraftingExclusionReason::OutputComponents(
                    components.keys().cloned().collect(),
                )));
            }
        }
        let output_id = resolver.item(string(result.get("id"))?)?;
        let count = match result.get("count").filter(|value| !value.is_null()) {
            None => 1,
            Some(value) => value
                .as_u64()
                .filter(|count| (1..=99).contains(count))
                .ok_or(ItemError::Invalid(
                    "crafting result count must be a JSON integer in 1..99",
                ))? as u8,
        };
        (output_id, count)
    };
    let output = ItemStack::new(output_id, count, ValidatedPatch::default())?;
    resolver.catalog.validate_stack(&output)?;
    if !resolver.catalog.supports_generic_click(&output)? {
        return Ok(Err(CraftingExclusionReason::OutputItemHook));
    }
    Ok(Ok(Recipe { pattern, output }))
}

fn validate_metadata(object: &Map<String, Value>) -> Result<(), ItemError> {
    // JsonOps treats JSON null as an absent optional field. These metadata do
    // not affect matching, but malformed values must not activate a recipe.
    if let Some(group) = object.get("group").filter(|value| !value.is_null())
        && !group.is_string()
    {
        return Err(ItemError::Invalid("crafting group must be a string"));
    }
    if let Some(category) = object.get("category").filter(|value| !value.is_null())
        && !matches!(
            category.as_str(),
            Some("building" | "redstone" | "equipment" | "misc")
        )
    {
        return Err(ItemError::Invalid("unknown crafting category"));
    }
    if let Some(notification) = object
        .get("show_notification")
        .filter(|value| !value.is_null())
        && !notification.is_boolean()
    {
        return Err(ItemError::Invalid(
            "crafting show_notification must be boolean",
        ));
    }
    Ok(())
}

fn parse_shaped(
    object: &Map<String, Value>,
    resolver: &mut TagResolver<'_>,
) -> Result<Result<Pattern, CraftingExclusionReason>, ItemError> {
    let raw = object
        .get("pattern")
        .and_then(Value::as_array)
        .ok_or(ItemError::Invalid("missing shaped pattern"))?;
    if raw.is_empty() || raw.len() > 3 {
        return Err(ItemError::Invalid("pattern height must be 1..3"));
    }
    let rows: Vec<Vec<char>> = raw
        .iter()
        .map(|value| {
            let row = string(Some(value))?;
            if row.encode_utf16().take(4).count() > 3 {
                return Err(ItemError::Invalid("pattern width exceeds 3"));
            }
            Ok(row.chars().collect())
        })
        .collect::<Result<_, _>>()?;
    let width = rows[0].len();
    if width == 0
        || width > 3
        || rows
            .iter()
            .any(|row| row.len() != width || row.iter().any(|symbol| symbol.len_utf16() != 1))
    {
        return Err(ItemError::Invalid(
            "pattern rows must have equal width 1..3",
        ));
    }
    let raw_key = object
        .get("key")
        .ok_or(ItemError::Invalid("missing shaped key"))?;
    let raw_key = self::object(raw_key)?;
    if raw_key.len() > 9 {
        return Err(ItemError::Limit("shaped key size"));
    }
    let mut key = BTreeMap::new();
    for (symbol, value) in raw_key {
        let mut chars = symbol.chars();
        let symbol = chars
            .next()
            .ok_or(ItemError::Invalid("empty shaped key symbol"))?;
        if symbol == ' ' || symbol.len_utf16() != 1 || chars.next().is_some() {
            return Err(ItemError::Invalid(
                "shaped key must be one non-space character",
            ));
        }
        key.insert(symbol, resolver.ingredient(value)?);
    }
    let used: BTreeSet<_> = rows
        .iter()
        .flatten()
        .copied()
        .filter(|symbol| *symbol != ' ')
        .collect();
    if used.is_empty() || used != key.keys().copied().collect() {
        return Err(ItemError::Invalid(
            "shaped pattern and key symbols disagree",
        ));
    }
    let min_y = rows
        .iter()
        .position(|row| row.iter().any(|symbol| *symbol != ' '))
        .ok_or(ItemError::Invalid("empty shaped pattern"))?;
    let max_y = rows
        .iter()
        .rposition(|row| row.iter().any(|symbol| *symbol != ' '))
        .ok_or(ItemError::Invalid("empty shaped pattern"))?;
    let min_x = (0..width)
        .find(|x| rows.iter().any(|row| row[*x] != ' '))
        .ok_or(ItemError::Invalid("empty shaped pattern"))?;
    let max_x = (0..width)
        .rfind(|x| rows.iter().any(|row| row[*x] != ' '))
        .ok_or(ItemError::Invalid("empty shaped pattern"))?;
    let (width, height) = (max_x - min_x + 1, max_y - min_y + 1);
    if width > 2 || height > 2 {
        return Ok(Err(CraftingExclusionReason::DoesNotFit));
    }
    let mut cells = Vec::with_capacity(width * height);
    for row in &rows[min_y..=max_y] {
        for symbol in &row[min_x..=max_x] {
            let ingredient = key.get(symbol);
            if let Some(ingredient) = ingredient {
                resolver.reserve(ingredient.len())?;
            }
            cells.push(ingredient.cloned());
        }
    }
    Ok(Ok(Pattern::Shaped {
        width,
        height,
        cells,
    }))
}

struct TagResolver<'a> {
    catalog: &'a ItemCatalog,
    tags: &'a BTreeMap<String, Value>,
    resolved: BTreeMap<String, ResolvedTag>,
    visiting: BTreeSet<String>,
    expanded: usize,
}

struct ResolvedTag {
    items: Ingredient,
    depth: usize,
}

impl TagResolver<'_> {
    fn reserve(&mut self, count: usize) -> Result<(), ItemError> {
        self.expanded = self
            .expanded
            .checked_add(count)
            .ok_or(ItemError::Limit("crafting ingredient expansion"))?;
        if self.expanded > MAX_VALUES {
            return Err(ItemError::Limit("crafting ingredient expansion"));
        }
        Ok(())
    }

    fn item(&self, name: &str) -> Result<ItemId, ItemError> {
        let name = identifier(name)?;
        if name == "minecraft:air" {
            return Err(ItemError::Invalid(
                "air is not a crafting ingredient or output",
            ));
        }
        self.catalog
            .item_id(&name)
            .ok_or(ItemError::Invalid("unknown crafting item"))
    }

    fn ingredient(&mut self, value: &Value) -> Result<Ingredient, ItemError> {
        if let Some(name) = value.as_str() {
            if let Some(tag) = name.strip_prefix('#') {
                let tag = identifier(tag)?;
                let values = self
                    .resolved
                    .get(&tag)
                    .ok_or(ItemError::Invalid("missing ingredient tag"))?;
                let count = values.items.len();
                self.reserve(count)?;
                return Ok(self.resolved[&tag].items.clone());
            }
            self.reserve(1)?;
            return Ok(vec![self.item(name)?]);
        }
        let values = value.as_array().ok_or(ItemError::Invalid(
            "ingredient must be an item, tag, or item array",
        ))?;
        if values.is_empty() || values.len() > MAX_ALTERNATIVES {
            return Err(ItemError::Limit("ingredient alternatives"));
        }
        self.reserve(values.len())?;
        let mut items = values
            .iter()
            .map(|value| self.item(string(Some(value))?))
            .collect::<Result<Vec<_>, _>>()?;
        items.sort_unstable();
        items.dedup();
        Ok(items)
    }

    fn resolve(&mut self, id: &str) -> Result<(), CraftingError> {
        if self.resolved.contains_key(id) {
            return Ok(());
        }
        self.resolve_inner(id).map_err(|source| CraftingError::Tag {
            tag_id: id.into(),
            source,
        })
    }

    fn resolve_inner(&mut self, id: &str) -> Result<(), ItemError> {
        if self.resolved.contains_key(id) {
            return Ok(());
        }
        if self.visiting.len() >= MAX_DEPTH {
            return Err(ItemError::Limit("crafting tag depth"));
        }
        if !self.visiting.insert(id.into()) {
            return Err(ItemError::Invalid("crafting tag cycle"));
        }
        let definition = self
            .tags
            .get(id)
            .ok_or(ItemError::Invalid("missing required crafting tag"))?;
        let definition = object(definition)?;
        if definition
            .keys()
            .any(|key| !["values", "replace"].contains(&key.as_str()))
        {
            return Err(ItemError::Invalid("unknown item tag field"));
        }
        if definition
            .get("replace")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(ItemError::Invalid("tag replace must be boolean"));
        }
        let entries = definition
            .get("values")
            .and_then(Value::as_array)
            .ok_or(ItemError::Invalid("missing tag values"))?;
        if entries.len() > MAX_ALTERNATIVES {
            return Err(ItemError::Limit("crafting tag entries"));
        }
        let mut items = BTreeSet::new();
        let mut depth = 1;
        for entry in entries {
            let (name, required) = if let Some(name) = entry.as_str() {
                (name, true)
            } else {
                let entry = object(entry)?;
                if entry
                    .keys()
                    .any(|key| !["id", "required"].contains(&key.as_str()))
                {
                    return Err(ItemError::Invalid("unknown item tag entry field"));
                }
                let required = match entry.get("required") {
                    None => true,
                    Some(value) => value
                        .as_bool()
                        .ok_or(ItemError::Invalid("tag required must be boolean"))?,
                };
                (string(entry.get("id"))?, required)
            };
            if let Some(nested) = name.strip_prefix('#') {
                let nested = identifier(nested)?;
                if !required && !self.tags.contains_key(&nested) {
                    continue;
                }
                self.resolve_inner(&nested)?;
                let nested_tag = &self.resolved[&nested];
                depth = depth.max(nested_tag.depth + 1);
                if depth > MAX_DEPTH {
                    return Err(ItemError::Limit("crafting tag depth"));
                }
                let count = nested_tag.items.len();
                self.reserve(count)?;
                items.extend(self.resolved[&nested].items.iter().copied());
            } else {
                let name = identifier(name)?;
                if !required && self.catalog.item_id(&name).is_none() {
                    continue;
                }
                self.reserve(1)?;
                items.insert(self.item(&name)?);
            }
        }
        self.visiting.remove(id);
        self.resolved.insert(
            id.into(),
            ResolvedTag {
                items: items.into_iter().collect(),
                depth,
            },
        );
        Ok(())
    }
}

#[derive(Default)]
struct JsonBudget {
    values: usize,
    text_bytes: usize,
}
impl JsonBudget {
    fn visit(&mut self, value: &Value, depth: usize) -> Result<(), ItemError> {
        self.values += 1;
        if depth > MAX_DEPTH || self.values > MAX_VALUES {
            return Err(ItemError::Limit("crafting JSON structure"));
        }
        match value {
            Value::String(value) => self.text(value.len())?,
            Value::Array(values) => {
                for value in values {
                    self.visit(value, depth + 1)?;
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    self.text(key.len())?;
                    self.visit(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn text(&mut self, len: usize) -> Result<(), ItemError> {
        self.text_bytes = self
            .text_bytes
            .checked_add(len)
            .ok_or(ItemError::Limit("crafting JSON text"))?;
        if self.text_bytes > MAX_TEXT_BYTES {
            return Err(ItemError::Limit("crafting JSON text"));
        }
        Ok(())
    }
}

fn identifier(name: &str) -> Result<String, ItemError> {
    if name.len() > 256 {
        return Err(ItemError::Invalid("crafting identifier too long"));
    }
    let normalized = if name.contains(':') {
        name.to_owned()
    } else {
        format!("minecraft:{name}")
    };
    if !crate::catalog::valid_identifier(&normalized) {
        return Err(ItemError::Invalid("invalid crafting identifier"));
    }
    Ok(normalized)
}
fn object(value: &Value) -> Result<&Map<String, Value>, ItemError> {
    value
        .as_object()
        .ok_or(ItemError::Invalid("expected crafting JSON object"))
}
fn string(value: Option<&Value>) -> Result<&str, ItemError> {
    value
        .and_then(Value::as_str)
        .ok_or(ItemError::Invalid("expected crafting JSON string"))
}

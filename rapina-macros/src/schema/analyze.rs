//! Semantic analysis for the schema macro.
//!
//! Two-pass analysis:
//! 1. Collect all entity names into a registry
//! 2. Resolve relationships and validate targets exist

use proc_macro2::Span;
use std::collections::HashSet;
use syn::{Ident, Result};

use super::parse::{EntityAttrs, EntityDef, FieldAttrs, FieldDef, RawFieldType, Schema};
use super::types::FieldType;

/// Analyzed schema with resolved relationships.
#[derive(Debug)]
pub struct AnalyzedSchema {
    pub entities: Vec<AnalyzedEntity>,
}

/// An entity with resolved field types.
#[derive(Debug)]
pub struct AnalyzedEntity {
    pub attrs: EntityAttrs,
    pub name: Ident,
    pub fields: Vec<AnalyzedField>,
    #[allow(dead_code)]
    pub span: Span,
}

/// A field with resolved type information.
#[derive(Debug)]
pub struct AnalyzedField {
    pub attrs: FieldAttrs,
    pub name: Ident,
    pub ty: FieldType,
    #[allow(dead_code)]
    pub span: Span,
    /// Whether this field gets `impl Related<Target>` in the generate stage.
    ///
    /// Assigned by [`validate_relation_rules`] once every field in the entity
    /// is resolved, since it depends on what the *other* fields target. Fields
    /// that lose the nomination get a generated `Linked` instead.
    pub implements_related: bool,
}

/// Entity registry for cross-reference validation.
struct EntityRegistry {
    names: HashSet<String>,
}

impl EntityRegistry {
    fn new(entities: &[EntityDef]) -> Self {
        let names = entities.iter().map(|e| e.name.to_string()).collect();
        EntityRegistry { names }
    }

    fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }
}

/// Analyze a parsed schema, resolving relationships and validating references.
pub fn analyze_schema(schema: Schema) -> Result<AnalyzedSchema> {
    // Check for duplicate entity names
    let mut seen_entities = HashSet::new();
    for entity in &schema.entities {
        let entity_name = entity.name.to_string();
        if !seen_entities.insert(entity_name.clone()) {
            return Err(syn::Error::new(
                entity.name.span(),
                format!("duplicate entity name '{}'", entity_name),
            ));
        }
    }

    // Build entity registry for cross-reference
    let registry = EntityRegistry::new(&schema.entities);

    // Analyze each entity
    let mut analyzed_entities = Vec::new();
    for entity in schema.entities {
        analyzed_entities.push(analyze_entity(entity, &registry)?);
    }

    Ok(AnalyzedSchema {
        entities: analyzed_entities,
    })
}

fn analyze_entity(entity: EntityDef, registry: &EntityRegistry) -> Result<AnalyzedEntity> {
    // Reject created_at/updated_at only when they'd collide with auto-generated timestamps
    for field in &entity.fields {
        let name = field.name.to_string();
        if name == "created_at" && entity.attrs.has_created_at {
            return Err(syn::Error::new(
                field.name.span(),
                "field 'created_at' is auto-generated. Use #[timestamps(none)] or #[timestamps(updated_at)] to declare it manually",
            ));
        }
        if name == "updated_at" && entity.attrs.has_updated_at {
            return Err(syn::Error::new(
                field.name.span(),
                "field 'updated_at' is auto-generated. Use #[timestamps(none)] or #[timestamps(created_at)] to declare it manually",
            ));
        }
    }

    let mut analyzed_fields = Vec::new();

    for field in entity.fields {
        analyzed_fields.push(analyze_field(field, registry)?);
    }

    // Decide which field owns `Related` for each target it references. Runs
    // once per entity, after every field is resolved, because the outcome
    // depends on what the other fields in this entity point at.
    validate_relation_rules(&entity.name, &mut analyzed_fields)?;

    // Validate custom primary key columns exist in the entity
    if let Some(ref pk_cols) = entity.attrs.primary_key {
        let field_names: HashSet<String> =
            analyzed_fields.iter().map(|f| f.name.to_string()).collect();

        for col in pk_cols {
            if !field_names.contains(col) {
                return Err(syn::Error::new(
                    entity.name.span(),
                    format!(
                        "primary_key column '{}' does not exist in entity '{}'",
                        col, entity.name
                    ),
                ));
            }
        }

        // Validate PK columns are scalar types (not relationships)
        for field in &analyzed_fields {
            let fname = field.name.to_string();
            if pk_cols.contains(&fname) && !matches!(field.ty, FieldType::Scalar { .. }) {
                return Err(syn::Error::new(
                    field.name.span(),
                    format!(
                        "primary_key column '{}' must be a scalar type, not a relationship",
                        fname
                    ),
                ));
            }
        }
    }

    Ok(AnalyzedEntity {
        attrs: entity.attrs,
        name: entity.name,
        fields: analyzed_fields,
        span: entity.span,
    })
}

fn analyze_field(field: FieldDef, registry: &EntityRegistry) -> Result<AnalyzedField> {
    let ty = match field.ty {
        RawFieldType::Scalar { scalar, optional } => FieldType::Scalar { scalar, optional },

        RawFieldType::Vec { inner } => {
            let inner_name = inner.to_string();

            // Vec<T> must reference an entity (has_many)
            if !registry.contains(&inner_name) {
                return Err(syn::Error::new(
                    inner.span(),
                    format!(
                        "unknown entity '{}' in Vec<{0}>. Did you define this entity?",
                        inner_name
                    ),
                ));
            }

            FieldType::HasMany { target: inner }
        }

        RawFieldType::Unknown { name, optional } => {
            let type_name = name.to_string();

            // If it's a known entity, it's a belongs_to relationship
            if registry.contains(&type_name) {
                FieldType::BelongsTo {
                    target: name,
                    optional,
                }
            } else {
                return Err(syn::Error::new(
                    name.span(),
                    format!(
                        "unknown type '{}'. Use a scalar type (String, i32, etc.) or reference a defined entity.",
                        type_name
                    ),
                ));
            }
        }
    };

    // Assume unambiguous: every relationship field gets `Related` unless
    // `validate_relation_rules` finds a competing field for the same target.
    let implements_related = !matches!(ty, FieldType::Scalar { .. });

    Ok(AnalyzedField {
        attrs: field.attrs,
        name: field.name,
        ty,
        span: field.span,
        implements_related,
    })
}

/// Decide which field owns `impl Related<Target>` for each target entity.
///
/// SeaORM's `Related<T>` is keyed on the type pair `(Self, T)` — it takes no
/// `self` and no field name — so it can only be implemented once per target.
/// Two fields referencing the same entity therefore need one of them nominated
/// as canonical; the rest fall back to a generated `Linked` (issue #678).
fn validate_relation_rules(entity: &Ident, fields: &mut [AnalyzedField]) -> Result<()> {
    // ensure #[related] is only on belongs_to fields, not scalars or has_many
    validate_related_attr_placement(fields)?;

    let mut error: Option<syn::Error> = None;

    for (target, members) in group_by_target(fields) {
        if let Err(e) = validate_target_group(entity, &target, &members, fields) {
            match error {
                Some(ref mut acc) => acc.combine(e),
                None => error = Some(e),
            }
        }
    }

    match error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Reject `#[related]` on fields that can never own a `Related` impl.
///
/// This has to run after type resolution rather than in the parser: `Vec<u8>`
/// and `Vec<Tag>` are syntactically identical, and only diverge once
/// `parse_field_type` sees the `u8` and resolves a `Scalar` instead of a
/// `HasMany`.
fn validate_related_attr_placement(fields: &[AnalyzedField]) -> Result<()> {
    for field in fields.iter() {
        if !field.attrs.related {
            continue;
        }

        match &field.ty {
            FieldType::BelongsTo { .. } => {}

            // A has_many carries no join columns of its own: SeaORM derives its
            // RelationDef by reversing the *target's* `Related` impl. Pointing
            // `Related` back at it would define `to()` in terms of `to()`,
            // which recurses forever when the target is this same entity.
            FieldType::HasMany { .. } => {
                return Err(syn::Error::new(
                    field.name.span(),
                    format!(
                        "#[related] cannot be used on the has_many field '{}'; mark the belongs_to field that owns the foreign key instead",
                        field.name
                    ),
                ));
            }

            FieldType::Scalar { .. } => {
                return Err(syn::Error::new(
                    field.name.span(),
                    format!(
                        "#[related] can only be used on a relationship field, but '{}' is a scalar",
                        field.name
                    ),
                ));
            }
        }
    }

    Ok(())
}

/// The entity a relationship field points at, or `None` for scalars.
fn relation_target(ty: &FieldType) -> Option<&Ident> {
    match ty {
        FieldType::BelongsTo { target, .. } | FieldType::HasMany { target } => Some(target),
        FieldType::Scalar { .. } => None,
    }
}

/// Group an entity's relationship field indices by the entity they target.
///
/// Both relationship kinds share a group, because `Related<T>` collides on the
/// target alone — a `belongs_to` and a `has_many` to the same entity conflict
/// just as two `belongs_to` do. Scalars are skipped. For:
///
/// ```ignore
/// Tx {
///     amount: i64,             // 0 — scalar, skipped
///     seller: Option<User>,    // 1 — belongs_to
///     from: Option<Account>,   // 2 — belongs_to
///     buyer: Option<User>,     // 3 — belongs_to
///     to: Option<Account>,     // 4 — belongs_to
///     watchers: Vec<User>,     // 5 — has_many, same group as 1 and 3
/// }
/// ```
///
/// this returns:
///
/// ```ignore
/// [
///     ("User".to_string(),    vec![1, 3, 5]),
///     ("Account".to_string(), vec![2, 4]),
/// ]
/// ```
///
/// `User` comes first because its first field (1) precedes `Account`'s (2), and
/// members are ascending within each group. That ordering changes nothing about
/// which field wins or the code generated — only the order errors are reported
/// in. A `HashMap` would make that vary between compiles of an identical file:
/// with neither group annotated, both are errors, and `Account`'s could print
/// first on one run and `User`'s on the next. A linear scan keeps them in source
/// order for free.
///
/// Indices are returned rather than references so the caller can take `&mut` on
/// the fields afterward to record the outcome.
fn group_by_target(fields: &[AnalyzedField]) -> Vec<(String, Vec<usize>)> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();

    for (idx, field) in fields.iter().enumerate() {
        let Some(target) = relation_target(&field.ty) else {
            continue;
        };
        let key = target.to_string();

        match groups.iter_mut().find(|(name, _)| *name == key) {
            Some((_, members)) => members.push(idx),
            None => groups.push((key, vec![idx])),
        }
    }

    groups
}

/// Check one target group against both relationship rules, then record which
/// of its fields owns `Related`.
///
/// The rules live in separate functions because a group can be wrong in two
/// independent ways, not because each applies to a different kind of group:
///
/// 1. [`validate_has_many_group`] — too many `has_many` fields, which cannot be
///    resolved at all.
/// 2. [`validate_belongs_to_group`] — too many `belongs_to` fields, which the
///    schema resolves by marking one `#[related]`.
///
/// Both run for every group.
fn validate_target_group(
    entity: &Ident,
    target: &str,
    members: &[usize],
    fields: &mut [AnalyzedField],
) -> Result<()> {
    // A single field referencing this target is unambiguous — it keeps the
    // `implements_related: true` that analyze_field assigned.
    if members.len() < 2 {
        return Ok(());
    }

    let (belongs_to, has_many): (Vec<usize>, Vec<usize>) = members
        .iter()
        .copied()
        .partition(|&i| matches!(fields[i].ty, FieldType::BelongsTo { .. }));

    // Rule 1 — has_many.
    validate_has_many_group(entity, target, &has_many, fields)?;

    // Rule 2 — belongs_to
    let winner = validate_belongs_to_group(entity, target, &belongs_to, fields)?;

    for &idx in members {
        fields[idx].implements_related = idx == winner;
    }

    Ok(())
}

/// Rule 1 — `has_many`: at most one may reference a given target.
///
/// A `has_many` carries no join columns of its own — SeaORM builds its
/// `RelationDef` by reversing the *target's* `Related` impl — so two of them
/// referencing one entity describe the identical join. Neither `Related` nor a
/// generated `Linked` can tell them apart, and both would quietly return the
/// same rows. Distinguishing them needs syntax naming the foreign key to
/// travel; until that exists the group is rejected.
///
/// This is where that syntax would be resolved once it exists.
fn validate_has_many_group(
    entity: &Ident,
    target: &str,
    has_many: &[usize],
    fields: &[AnalyzedField],
) -> Result<()> {
    if has_many.len() < 2 {
        return Ok(());
    }

    Err(syn::Error::new(
        entity.span(),
        format!(
            "entity '{}' has {} has_many fields referencing '{}' ({}); this is not supported yet — SeaORM cannot distinguish them without an explicit foreign key",
            entity,
            has_many.len(),
            target,
            field_list(has_many, fields),
        ),
    ))
}

/// Rule 2 — `belongs_to`: exactly one of them owns `Related` for this target.
///
/// A single candidate wins outright, since there is nothing to choose between.
/// Two or more require `#[related]` on exactly one.
///
/// Returns the index of the winning field. `belongs_to` must be non-empty;
/// [`validate_target_group`] guarantees that by applying rule 1 first.
fn validate_belongs_to_group(
    entity: &Ident,
    target: &str,
    belongs_to: &[usize],
    fields: &[AnalyzedField],
) -> Result<usize> {
    // One eligible field means there is no choice to make, so no annotation is
    // required. This is what lets a mixed group such as
    // `Category { parent: Option<Category>, children: Vec<Category> }` resolve
    // without the user writing anything new.
    if belongs_to.len() == 1 {
        return Ok(belongs_to[0]);
    }

    let marked: Vec<usize> = belongs_to
        .iter()
        .copied()
        .filter(|&i| fields[i].attrs.related)
        .collect();

    // how many fields are marked #[related] in this group? 0, 1, or more than 1?
    match marked.as_slice() {
        [winner] => Ok(*winner),

        [] => Err(syn::Error::new(
            entity.span(),
            format!(
                "entity '{}' has {} fields referencing '{}' ({}); add #[related] to exactly one of them to choose which one find_related uses",
                entity,
                belongs_to.len(),
                target,
                field_list(belongs_to, fields),
            ),
        )),

        // Report every surplus mark, not just the first, so one compile shows
        // all of them.
        [first, second, rest @ ..] => {
            let claimed_by = fields[*first].name.to_string();
            let surplus = |idx: usize| {
                syn::Error::new(
                    fields[idx].name.span(),
                    format!(
                        "only one field per target may be marked #[related]; '{}' is already claimed by '{}'",
                        target, claimed_by
                    ),
                )
            };

            let mut error = surplus(*second);
            for &idx in rest {
                error.combine(surplus(idx));
            }
            Err(error)
        }
    }
}

/// Render field names for an error message: `'from', 'to'`.
fn field_list(indices: &[usize], fields: &[AnalyzedField]) -> String {
    indices
        .iter()
        .map(|&i| format!("'{}'", fields[i].name))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::parse::parse_schema;
    use quote::quote;

    #[test]
    fn test_analyze_simple_schema() {
        let input = quote! {
            User {
                email: String,
                name: String,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let analyzed = analyze_schema(parsed).unwrap();

        assert_eq!(analyzed.entities.len(), 1);
        assert_eq!(analyzed.entities[0].fields.len(), 2);
    }

    #[test]
    fn test_analyze_has_many_relationship() {
        let input = quote! {
            User {
                posts: Vec<Post>,
            }

            Post {
                title: String,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let analyzed = analyze_schema(parsed).unwrap();

        let user = &analyzed.entities[0];
        assert!(matches!(user.fields[0].ty, FieldType::HasMany { .. }));
    }

    #[test]
    fn test_analyze_belongs_to_relationship() {
        let input = quote! {
            User {
                email: String,
            }

            Post {
                author: User,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let analyzed = analyze_schema(parsed).unwrap();

        let post = &analyzed.entities[1];
        assert!(matches!(
            post.fields[0].ty,
            FieldType::BelongsTo {
                optional: false,
                ..
            }
        ));
    }

    #[test]
    fn test_analyze_optional_belongs_to() {
        let input = quote! {
            User {
                email: String,
            }

            Comment {
                author: Option<User>,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let analyzed = analyze_schema(parsed).unwrap();

        let comment = &analyzed.entities[1];
        assert!(matches!(
            comment.fields[0].ty,
            FieldType::BelongsTo { optional: true, .. }
        ));
    }

    #[test]
    fn test_unknown_entity_in_vec_error() {
        let input = quote! {
            User {
                posts: Vec<UnknownEntity>,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let result = analyze_schema(parsed);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown entity"));
    }

    #[test]
    fn test_unknown_type_error() {
        let input = quote! {
            User {
                foo: UnknownType,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let result = analyze_schema(parsed);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown type"));
    }

    #[test]
    fn test_duplicate_entity_error() {
        let input = quote! {
            User {
                email: String,
            }

            User {
                name: String,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let result = analyze_schema(parsed);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate entity"));
    }

    #[test]
    fn test_analyze_preserves_entity_attrs() {
        let input = quote! {
            #[table_name = "people"]
            Person {
                name: String,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let analyzed = analyze_schema(parsed).unwrap();

        assert_eq!(
            analyzed.entities[0].attrs.table_name,
            Some("people".to_string())
        );
    }

    #[test]
    fn test_created_at_allowed_with_timestamps_none() {
        let input = quote! {
            #[timestamps(none)]
            User {
                email: String,
                created_at: NaiveDateTime,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let result = analyze_schema(parsed);
        assert!(result.is_ok());
    }

    #[test]
    fn test_created_at_rejected_with_default_timestamps() {
        let input = quote! {
            User {
                email: String,
                created_at: NaiveDateTime,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let result = analyze_schema(parsed);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("auto-generated"));
    }

    #[test]
    fn test_analyze_preserves_field_attrs() {
        let input = quote! {
            User {
                #[unique]
                #[column = "user_email"]
                email: String,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let analyzed = analyze_schema(parsed).unwrap();

        let field = &analyzed.entities[0].fields[0];
        assert!(field.attrs.unique);
        assert_eq!(field.attrs.column_name, Some("user_email".to_string()));
    }

    #[test]
    fn test_analyze_composite_primary_key() {
        let input = quote! {
            #[primary_key(user_id, role_id)]
            #[timestamps(none)]
            UsersRole {
                user_id: i32,
                role_id: i32,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let analyzed = analyze_schema(parsed).unwrap();

        let entity = &analyzed.entities[0];
        assert_eq!(
            entity.attrs.primary_key,
            Some(vec!["user_id".to_string(), "role_id".to_string()])
        );
    }

    #[test]
    fn test_analyze_primary_key_column_not_found() {
        let input = quote! {
            #[primary_key(user_id, nonexistent)]
            #[timestamps(none)]
            UsersRole {
                user_id: i32,
                role_id: i32,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let result = analyze_schema(parsed);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_analyze_primary_key_must_be_scalar() {
        let input = quote! {
            User {
                email: String,
            }

            #[primary_key(author)]
            #[timestamps(none)]
            Post {
                author: User,
                title: String,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let result = analyze_schema(parsed);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("scalar type"));
    }

    #[test]
    fn test_analyze_primary_key_with_extra_fields() {
        let input = quote! {
            #[primary_key(user_id, role_id)]
            #[timestamps(none)]
            UsersRole {
                user_id: i32,
                role_id: i32,
                assigned_at: NaiveDateTime,
            }
        };

        let parsed = parse_schema(input).unwrap();
        let analyzed = analyze_schema(parsed).unwrap();

        let entity = &analyzed.entities[0];
        assert_eq!(entity.fields.len(), 3);
        assert_eq!(
            entity.attrs.primary_key,
            Some(vec!["user_id".to_string(), "role_id".to_string()])
        );
    }

    // ---- issue #678: ambiguous relationships and #[related] ----

    /// Analyze a schema and return the entity at `idx`, or panic on error.
    fn analyze_ok(input: proc_macro2::TokenStream) -> AnalyzedSchema {
        analyze_schema(parse_schema(input).unwrap()).unwrap()
    }

    /// Analyze a schema expected to fail, returning every error message.
    ///
    /// `syn::Error::to_string` only renders the first error of a combined set,
    /// so iterate to see all of them.
    fn analyze_err(input: proc_macro2::TokenStream) -> String {
        analyze_schema(parse_schema(input).unwrap())
            .unwrap_err()
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// `implements_related` for each field of an entity, in declaration order.
    fn related_flags(entity: &AnalyzedEntity) -> Vec<bool> {
        entity.fields.iter().map(|f| f.implements_related).collect()
    }

    #[test]
    fn test_group_by_target_is_source_ordered() {
        let analyzed = analyze_ok(quote! {
            Account { name: String, }
            User { email: String, }

            Tx {
                amount: i64,
                seller: Option<User>,
                #[related] from: Option<Account>,
                buyer: Option<User>,
                to: Option<Account>,
                #[related] primary: Option<User>,
            }
        });

        let groups = group_by_target(&analyzed.entities[2].fields);

        // Scalars are skipped, groups appear in first-appearance order, and
        // members are ascending within a group.
        assert_eq!(
            groups,
            vec![
                ("User".to_string(), vec![1, 3, 5]),
                ("Account".to_string(), vec![2, 4]),
            ]
        );
    }

    #[test]
    fn test_single_belongs_to_implements_related() {
        let analyzed = analyze_ok(quote! {
            User { email: String, }
            Post { title: String, author: User, }
        });

        assert_eq!(related_flags(&analyzed.entities[1]), vec![false, true]);
    }

    #[test]
    fn test_related_attr_wins_regardless_of_declaration_order() {
        // `to` is declared second and still wins — the choice is the attribute,
        // not the position.
        let analyzed = analyze_ok(quote! {
            Account { name: String, }
            Tx {
                from: Option<Account>,
                #[related]
                to: Option<Account>,
            }
        });

        assert_eq!(related_flags(&analyzed.entities[1]), vec![false, true]);
    }

    #[test]
    fn test_ambiguous_belongs_to_without_related_errors() {
        let err = analyze_err(quote! {
            Account { name: String, }
            Tx {
                from: Option<Account>,
                to: Option<Account>,
            }
        });

        assert!(err.contains("'Tx'"), "{err}");
        assert!(err.contains("'Account'"), "{err}");
        assert!(err.contains("'from'"), "{err}");
        assert!(err.contains("'to'"), "{err}");
        assert!(err.contains("#[related]"), "{err}");
    }

    #[test]
    fn test_multiple_related_attrs_error() {
        let err = analyze_err(quote! {
            Account { name: String, }
            Tx {
                #[related]
                from: Option<Account>,
                #[related]
                to: Option<Account>,
            }
        });

        assert!(err.contains("only one field per target"), "{err}");
        assert!(err.contains("'from'"), "{err}");
    }

    #[test]
    fn test_three_belongs_to_one_marked() {
        let analyzed = analyze_ok(quote! {
            Warehouse { name: String, }
            Shipment {
                origin: Warehouse,
                #[related]
                destination: Warehouse,
                backup: Option<Warehouse>,
            }
        });

        assert_eq!(
            related_flags(&analyzed.entities[1]),
            vec![false, true, false]
        );
    }

    #[test]
    fn test_mixed_self_referential_group_needs_no_attr() {
        // One belongs_to candidate, so there is nothing to disambiguate and no
        // annotation is required. `parent` must win: pointing Related at the
        // has_many would define `to()` in terms of itself.
        let analyzed = analyze_ok(quote! {
            Category {
                name: String,
                parent: Option<Category>,
                children: Vec<Category>,
            }
        });

        assert_eq!(
            related_flags(&analyzed.entities[0]),
            vec![false, true, false]
        );
    }

    #[test]
    fn test_mixed_group_not_self_referential() {
        let analyzed = analyze_ok(quote! {
            Tag { label: String, }
            Post {
                title: String,
                primary_tag: Option<Tag>,
                tags: Vec<Tag>,
            }
        });

        assert_eq!(
            related_flags(&analyzed.entities[1]),
            vec![false, true, false]
        );
    }

    #[test]
    fn test_multiple_has_many_to_same_target_errors() {
        let err = analyze_err(quote! {
            Account {
                name: String,
                txs_sent: Vec<Tx>,
                txs_received: Vec<Tx>,
            }
            Tx { amount: i64, account: Account, }
        });

        assert!(err.contains("has_many"), "{err}");
        assert!(err.contains("not supported yet"), "{err}");
        assert!(err.contains("'txs_sent'"), "{err}");
        assert!(err.contains("'txs_received'"), "{err}");
    }

    #[test]
    fn test_multiple_has_many_errors_even_alongside_a_belongs_to() {
        // The has_many rule must not be skipped just because the group also
        // contains a nominable belongs_to. Without this, `primary_tx` would win
        // and the two has_many fields would each get a Linked resolving to the
        // exact same join, silently returning identical rows.
        let err = analyze_err(quote! {
            Account {
                name: String,
                txs_sent: Vec<Tx>,
                txs_received: Vec<Tx>,
                primary_tx: Option<Tx>,
            }
            Tx { amount: i64, account: Account, }
        });

        assert!(err.contains("not supported yet"), "{err}");
        assert!(err.contains("'txs_sent'"), "{err}");
        assert!(err.contains("'txs_received'"), "{err}");
    }

    #[test]
    fn test_one_has_many_alongside_two_belongs_to_still_needs_related() {
        // A single has_many is fine; the belongs_to rule still applies.
        let analyzed = analyze_ok(quote! {
            Account {
                name: String,
                txs: Vec<Tx>,
                #[related]
                primary_tx: Option<Tx>,
                backup_tx: Option<Tx>,
            }
            Tx { amount: i64, account: Account, }
        });

        assert_eq!(
            related_flags(&analyzed.entities[0]),
            vec![false, false, true, false]
        );
    }

    #[test]
    fn test_related_on_scalar_errors() {
        let err = analyze_err(quote! {
            User {
                #[related]
                email: String,
            }
        });

        assert!(
            err.contains("can only be used on a relationship field"),
            "{err}"
        );
        assert!(err.contains("'email'"), "{err}");
    }

    #[test]
    fn test_related_on_bytes_vec_errors() {
        // `Vec<u8>` is indistinguishable from `Vec<Entity>` at parse time, so
        // this can only be caught after type resolution.
        let err = analyze_err(quote! {
            User {
                #[related]
                avatar: Vec<u8>,
            }
        });

        assert!(err.contains("is a scalar"), "{err}");
        assert!(err.contains("'avatar'"), "{err}");
    }

    #[test]
    fn test_related_on_has_many_errors() {
        let err = analyze_err(quote! {
            Category {
                name: String,
                parent: Option<Category>,
                #[related]
                children: Vec<Category>,
            }
        });

        assert!(err.contains("has_many field 'children'"), "{err}");
        assert!(err.contains("belongs_to"), "{err}");
    }

    #[test]
    fn test_separate_entities_targeting_same_entity_is_not_ambiguous() {
        // Grouping is per-entity: two entities each with one relation to User
        // must not be treated as a conflict.
        let analyzed = analyze_ok(quote! {
            User { email: String, }
            Post { title: String, author: User, }
            Comment { body: Text, author: User, }
        });

        assert_eq!(related_flags(&analyzed.entities[1]), vec![false, true]);
        assert_eq!(related_flags(&analyzed.entities[2]), vec![false, true]);
    }

    #[test]
    fn test_related_on_unambiguous_field_is_a_noop() {
        let analyzed = analyze_ok(quote! {
            User { email: String, }
            Post {
                title: String,
                #[related]
                author: User,
            }
        });

        assert_eq!(related_flags(&analyzed.entities[1]), vec![false, true]);
    }

    #[test]
    fn test_two_broken_groups_both_reported() {
        let err = analyze_err(quote! {
            Account { name: String, }
            User { email: String, }
            Tx {
                from: Option<Account>,
                to: Option<Account>,
                seller: Option<User>,
                buyer: Option<User>,
            }
        });

        assert!(err.contains("'Account'"), "{err}");
        assert!(err.contains("'User'"), "{err}");
    }
}

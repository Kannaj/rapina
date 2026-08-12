//! Integration tests for the schema! macro.
//!
//! These tests verify that the generated code compiles and matches SeaORM patterns.

#![cfg(feature = "database")]

use rapina::prelude::*;
use rapina::sea_orm::entity::prelude::*;

// Define a test schema with various relationship types
schema! {
    TestUser {
        email: String,
        name: String,
        bio: Option<Text>,
        posts: Vec<TestPost>,
        comments: Vec<TestComment>,
    }

    TestPost {
        title: String,
        content: Text,
        published: bool,
        author: TestUser,
        comments: Vec<TestComment>,
    }

    TestComment {
        content: Text,
        post: TestPost,
        author: Option<TestUser>,
    }
}

#[test]
fn test_user_model_compiles() {
    use test_user::Model;

    // Verify the Model struct has the expected fields
    let user = Model {
        id: 1,
        email: "test@example.com".to_string(),
        name: "Test User".to_string(),
        bio: Some("A test user".to_string()),
        created_at: DateTimeUtc::default(),
        updated_at: DateTimeUtc::default(),
    };

    assert_eq!(user.id, 1);
    assert_eq!(user.email, "test@example.com");
}

#[test]
fn test_post_model_has_foreign_key() {
    use test_post::Model;

    // Verify the belongs_to relationship generates author_id
    let post = Model {
        id: 1,
        title: "Test Post".to_string(),
        content: "Test content".to_string(),
        published: true,
        author_id: 1, // Foreign key from belongs_to
        created_at: DateTimeUtc::default(),
        updated_at: DateTimeUtc::default(),
    };

    assert_eq!(post.author_id, 1);
}

#[test]
fn test_comment_model_has_optional_foreign_key() {
    use test_comment::Model;

    // Verify optional belongs_to generates Option<i32> FK
    let comment_with_author = Model {
        id: 1,
        content: "Great post!".to_string(),
        post_id: 1,
        author_id: Some(1), // Optional FK
        created_at: DateTimeUtc::default(),
        updated_at: DateTimeUtc::default(),
    };

    let comment_without_author = Model {
        id: 2,
        content: "Anonymous comment".to_string(),
        post_id: 1,
        author_id: None,
        created_at: DateTimeUtc::default(),
        updated_at: DateTimeUtc::default(),
    };

    assert_eq!(comment_with_author.author_id, Some(1));
    assert_eq!(comment_without_author.author_id, None);
}

#[test]
fn test_relation_enum_exists() {
    // Verify Relation enums are generated with expected variants
    use test_comment::Relation as CommentRelation;
    use test_post::Relation as PostRelation;
    use test_user::Relation as UserRelation;

    // User has Posts and Comments (has_many)
    let _ = UserRelation::Posts;
    let _ = UserRelation::Comments;

    // Post has Author (belongs_to) and Comments (has_many)
    let _ = PostRelation::Author;
    let _ = PostRelation::Comments;

    // Comment has Post (belongs_to) and Author (optional belongs_to)
    let _ = CommentRelation::Post;
    let _ = CommentRelation::Author;
}

#[test]
fn test_entity_traits_implemented() {
    // Verify Entity trait is implemented via EntityName
    let _ = test_user::Entity::table_name(&test_user::Entity);
    let _ = test_post::Entity::table_name(&test_post::Entity);
    let _ = test_comment::Entity::table_name(&test_comment::Entity);
}

// ---------------------------------------------------------------------------
// Issue #678: several relationship fields referencing the same target entity.
//
// `Related<T>` can only be implemented once per target, so these schemas used
// to fail to compile with E0119. `#[related]` nominates the field that keeps
// `Related`; the rest get a generated `Linked`. This whole file failing to
// compile is itself the regression signal.
// ---------------------------------------------------------------------------

schema! {
    Account {
        name: String,
    }

    RegressionTx {
        amount: i64,
        from: Option<Account>,
        // Deliberately not the first-declared field: the nomination follows the
        // attribute, not declaration order.
        #[related]
        to: Option<Account>,
    }
}

#[test]
fn test_related_attr_selects_the_marked_field() {
    use rapina::sea_orm::{DbBackend, QueryTrait};

    let stmt = regression_tx::Entity::find()
        .find_also_related(account::Entity)
        .build(DbBackend::Sqlite)
        .to_string();

    assert!(
        stmt.contains(r#"JOIN "accounts" ON "regression_txs"."to_id""#),
        "{stmt}"
    );
}

#[test]
fn test_unmarked_field_is_reachable_via_linked() {
    use rapina::sea_orm::{DbBackend, ModelTrait, QueryTrait};

    let tx = regression_tx::Model {
        id: 1,
        amount: 500,
        from_id: Some(1),
        to_id: Some(2),
        created_at: DateTimeUtc::default(),
        updated_at: DateTimeUtc::default(),
    };

    let stmt = tx
        .find_linked(regression_tx::FromLink)
        .build(DbBackend::Sqlite)
        .to_string();

    assert!(
        stmt.contains(r#"ON "r0"."from_id" = "accounts"."id""#),
        "{stmt}"
    );
}

// Three fields to one target: the losing fields each need their own Linked,
// resolving to their own foreign key rather than collapsing onto the winner's.
schema! {
    Warehouse {
        name: String,
    }

    Shipment {
        weight: i64,
        origin: Warehouse,
        #[related]
        destination: Warehouse,
        backup: Option<Warehouse>,
    }
}

#[test]
fn test_three_way_ambiguity_resolves_each_field_separately() {
    use rapina::sea_orm::{DbBackend, ModelTrait, QueryTrait};

    let stmt = shipment::Entity::find()
        .find_also_related(warehouse::Entity)
        .build(DbBackend::Sqlite)
        .to_string();
    assert!(
        stmt.contains(r#"JOIN "warehouses" ON "shipments"."destination_id""#),
        "{stmt}"
    );

    let shipment = shipment::Model {
        id: 1,
        weight: 100,
        origin_id: 1,
        destination_id: 2,
        backup_id: Some(3),
        created_at: DateTimeUtc::default(),
        updated_at: DateTimeUtc::default(),
    };

    let origin = shipment
        .find_linked(shipment::OriginLink)
        .build(DbBackend::Sqlite)
        .to_string();
    assert!(origin.contains(r#"ON "r0"."origin_id""#), "{origin}");

    let backup = shipment
        .find_linked(shipment::BackupLink)
        .build(DbBackend::Sqlite)
        .to_string();
    assert!(backup.contains(r#"ON "r0"."backup_id""#), "{backup}");
}

// Self-referential ambiguity: the target entity is the declaring entity.
schema! {
    Employee {
        name: String,
        #[related]
        manager: Option<Employee>,
        mentor: Option<Employee>,
    }
}

#[test]
fn test_self_referential_ambiguity() {
    use rapina::sea_orm::{DbBackend, ModelTrait, QueryTrait};

    let stmt = employee::Entity::find()
        .find_also_related(employee::Entity)
        .build(DbBackend::Sqlite)
        .to_string();
    assert!(stmt.contains(r#""employees"."manager_id""#), "{stmt}");

    let employee = employee::Model {
        id: 1,
        name: "Alice".to_string(),
        manager_id: Some(2),
        mentor_id: Some(3),
        created_at: DateTimeUtc::default(),
        updated_at: DateTimeUtc::default(),
    };

    let stmt = employee
        .find_linked(employee::MentorLink)
        .build(DbBackend::Sqlite)
        .to_string();
    assert!(stmt.contains(r#"ON "r0"."mentor_id""#), "{stmt}");
}

// A mixed belongs_to/has_many group has only one nominable field, so it needs
// no annotation. This also guards against infinite recursion: pointing
// `Related` at the has_many side would make `to()` call itself, and the
// resulting stack overflow aborts the test binary rather than failing softly.
schema! {
    Category {
        name: String,
        parent: Option<Category>,
        children: Vec<Category>,
    }
}

#[test]
fn test_mixed_self_referential_group_needs_no_annotation() {
    use rapina::sea_orm::{DbBackend, ModelTrait, QueryTrait};

    let category = category::Model {
        id: 3,
        name: "rust".to_string(),
        parent_id: Some(1),
        created_at: DateTimeUtc::default(),
        updated_at: DateTimeUtc::default(),
    };

    // parent wins the nomination automatically. (The table name is the naive
    // pluralization the generator already applies, not a typo.)
    let parent = category
        .find_related(category::Entity)
        .build(DbBackend::Sqlite)
        .to_string();
    assert!(
        parent.contains(r#"ON "categorys"."parent_id" = "categorys"."id""#),
        "{parent}"
    );

    // children resolves to the reverse of that same edge.
    let children = category
        .find_linked(category::ChildrenLink)
        .build(DbBackend::Sqlite)
        .to_string();
    assert!(
        children.contains(r#"ON "r0"."id" = "categorys"."parent_id""#),
        "{children}"
    );
}

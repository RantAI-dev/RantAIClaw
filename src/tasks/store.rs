// src/tasks/store.rs
use crate::config::Config;
use crate::tasks::types::*;
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use uuid::Uuid;

/// Open (or create) the tasks SQLite database.
/// Uses WAL mode for concurrent read performance.
fn open_db(config: &Config) -> Result<Connection> {
    let db_path = config.workspace_dir.join("tasks.db");
    std::fs::create_dir_all(&config.workspace_dir)?;
    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open tasks DB at {}", db_path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    init_tables(&conn)?;
    Ok(conn)
}

fn init_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            organization_id TEXT,
            title TEXT NOT NULL,
            description TEXT,
            status TEXT NOT NULL DEFAULT 'TODO',
            priority TEXT NOT NULL DEFAULT 'MEDIUM',
            assignee_id TEXT,
            group_id TEXT,
            reviewer_id TEXT,
            human_review INTEGER NOT NULL DEFAULT 0,
            review_status TEXT,
            review_comment TEXT,
            parent_task_id TEXT REFERENCES tasks(id) ON DELETE CASCADE,
            created_by_employee_id TEXT,
            created_by_user_id TEXT,
            due_date TEXT,
            completed_at TEXT,
            order_in_status INTEGER NOT NULL DEFAULT 0,
            order_in_parent INTEGER NOT NULL DEFAULT 0,
            metadata TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_tasks_org_status ON tasks(organization_id, status);
        CREATE INDEX IF NOT EXISTS idx_tasks_assignee_status ON tasks(assignee_id, status);
        CREATE INDEX IF NOT EXISTS idx_tasks_group ON tasks(group_id);
        CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_task_id);

        CREATE TABLE IF NOT EXISTS task_comments (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            content TEXT NOT NULL,
            author_type TEXT NOT NULL,
            author_employee_id TEXT,
            author_user_id TEXT,
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_task_comments_task ON task_comments(task_id, created_at);

        CREATE TABLE IF NOT EXISTS task_events (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            event_type TEXT NOT NULL,
            actor_type TEXT NOT NULL,
            actor_employee_id TEXT,
            actor_user_id TEXT,
            data TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_task_events_task ON task_events(task_id, created_at);",
    )
    .context("Failed to initialize tasks tables")?;
    Ok(())
}

fn with_connection<F, T>(config: &Config, f: F) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T>,
{
    let conn = open_db(config)?;
    f(&conn)
}

// ── Task CRUD ────────────────────────────────────────────────

pub fn create_task(config: &Config, input: &CreateTask) -> Result<Task> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let priority = input.priority.unwrap_or_default();
    let human_review = input.human_review.unwrap_or(false);
    let metadata = input
        .metadata
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()))
        .unwrap_or_else(|| "{}".into());

    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO tasks (
                id, organization_id, title, description, status, priority,
                assignee_id, group_id, reviewer_id, human_review,
                parent_task_id, created_by_employee_id, created_by_user_id,
                due_date, order_in_status, order_in_parent, metadata,
                created_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10,
                ?11, ?12, ?13,
                ?14, 0, 0, ?15,
                ?16, ?17
            )",
            params![
                id,
                input.organization_id,
                input.title,
                input.description,
                TaskStatus::Todo.as_str(),
                priority.as_str(),
                input.assignee_id,
                input.group_id,
                input.reviewer_id,
                human_review,
                input.parent_task_id,
                input.created_by_employee_id,
                input.created_by_user_id,
                input.due_date.map(|d| d.to_rfc3339()),
                metadata,
                now.to_rfc3339(),
                now.to_rfc3339(),
            ],
        )
        .context("Failed to insert task")?;

        // Record creation event
        let event_id = Uuid::new_v4().to_string();
        let actor_type = if input.created_by_employee_id.is_some() {
            "EMPLOYEE"
        } else {
            "HUMAN"
        };
        conn.execute(
            "INSERT INTO task_events (id, task_id, event_type, actor_type, actor_employee_id, actor_user_id, data, created_at)
             VALUES (?1, ?2, 'CREATED', ?3, ?4, ?5, '{}', ?6)",
            params![
                event_id,
                id,
                actor_type,
                input.created_by_employee_id,
                input.created_by_user_id,
                now.to_rfc3339(),
            ],
        )?;

        Ok(())
    })?;

    get_task(config, &id)
}

pub fn get_task(config: &Config, id: &str) -> Result<Task> {
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, organization_id, title, description, status, priority,
                    assignee_id, group_id, reviewer_id, human_review,
                    review_status, review_comment, parent_task_id,
                    created_by_employee_id, created_by_user_id,
                    due_date, completed_at, order_in_status, order_in_parent,
                    metadata, created_at, updated_at
             FROM tasks WHERE id = ?1",
        )?;
        stmt.query_row(params![id], row_to_task)
            .with_context(|| format!("Task not found: {id}"))
    })
}

pub fn list_tasks(config: &Config, filter: &TaskFilter) -> Result<Vec<Task>> {
    with_connection(config, |conn| {
        let mut sql = String::from(
            "SELECT id, organization_id, title, description, status, priority,
                    assignee_id, group_id, reviewer_id, human_review,
                    review_status, review_comment, parent_task_id,
                    created_by_employee_id, created_by_user_id,
                    due_date, completed_at, order_in_status, order_in_parent,
                    metadata, created_at, updated_at
             FROM tasks WHERE 1=1",
        );
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ref status) = filter.status {
            param_values.push(Box::new(status.as_str().to_string()));
            sql.push_str(&format!(" AND status = ?{}", param_values.len()));
        }
        if let Some(ref assignee) = filter.assignee_id {
            param_values.push(Box::new(assignee.clone()));
            sql.push_str(&format!(" AND assignee_id = ?{}", param_values.len()));
        }
        if let Some(ref group) = filter.group_id {
            param_values.push(Box::new(group.clone()));
            sql.push_str(&format!(" AND group_id = ?{}", param_values.len()));
        }
        if let Some(ref priority) = filter.priority {
            param_values.push(Box::new(priority.as_str().to_string()));
            sql.push_str(&format!(" AND priority = ?{}", param_values.len()));
        }
        if let Some(ref parent) = filter.parent_task_id {
            param_values.push(Box::new(parent.clone()));
            sql.push_str(&format!(" AND parent_task_id = ?{}", param_values.len()));
        }
        if filter.top_level_only == Some(true) {
            sql.push_str(" AND parent_task_id IS NULL");
        }
        if let Some(ref org) = filter.organization_id {
            param_values.push(Box::new(org.clone()));
            sql.push_str(&format!(" AND organization_id = ?{}", param_values.len()));
        }

        sql.push_str(" ORDER BY order_in_status ASC, created_at DESC");

        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        if let Some(offset) = filter.offset {
            sql.push_str(&format!(" OFFSET {offset}"));
        }

        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let tasks = stmt
            .query_map(params_ref.as_slice(), row_to_task)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(tasks)
    })
}

pub fn update_task(config: &Config, id: &str, patch: &TaskPatch) -> Result<Task> {
    let now = Utc::now();

    with_connection(config, |conn| {
        // Build dynamic UPDATE
        let mut sets = vec!["updated_at = ?1".to_string()];
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(now.to_rfc3339())];

        macro_rules! maybe_set {
            ($field:ident, $col:literal) => {
                if let Some(ref val) = patch.$field {
                    param_values.push(Box::new(val.clone()));
                    sets.push(format!("{} = ?{}", $col, param_values.len()));
                }
            };
        }

        macro_rules! maybe_set_option {
            ($field:ident, $col:literal, $map:expr) => {
                if let Some(ref opt) = patch.$field {
                    match opt {
                        Some(val) => {
                            param_values.push(Box::new($map(val)));
                            sets.push(format!("{} = ?{}", $col, param_values.len()));
                        }
                        None => {
                            sets.push(format!("{} = NULL", $col));
                        }
                    }
                }
            };
        }

        maybe_set!(title, "title");
        maybe_set!(description, "description");
        maybe_set!(order_in_status, "order_in_status");
        maybe_set!(order_in_parent, "order_in_parent");

        if let Some(ref status) = patch.status {
            param_values.push(Box::new(status.as_str().to_string()));
            sets.push(format!("status = ?{}", param_values.len()));
            if *status == TaskStatus::Done {
                param_values.push(Box::new(now.to_rfc3339()));
                sets.push(format!("completed_at = ?{}", param_values.len()));
            } else {
                // Clear completed_at when transitioning away from Done
                sets.push("completed_at = NULL".to_string());
            }
        }
        if let Some(ref priority) = patch.priority {
            param_values.push(Box::new(priority.as_str().to_string()));
            sets.push(format!("priority = ?{}", param_values.len()));
        }
        if let Some(ref human_review) = patch.human_review {
            param_values.push(Box::new(*human_review));
            sets.push(format!("human_review = ?{}", param_values.len()));
        }
        if let Some(ref metadata) = patch.metadata {
            param_values.push(Box::new(serde_json::to_string(metadata)?));
            sets.push(format!("metadata = ?{}", param_values.len()));
        }

        maybe_set_option!(assignee_id, "assignee_id", |v: &String| v.clone());
        maybe_set_option!(group_id, "group_id", |v: &String| v.clone());
        maybe_set_option!(reviewer_id, "reviewer_id", |v: &String| v.clone());
        maybe_set_option!(review_status, "review_status", |v: &ReviewStatus| v
            .as_str()
            .to_string());
        maybe_set_option!(review_comment, "review_comment", |v: &String| v.clone());
        maybe_set_option!(due_date, "due_date", |v: &chrono::DateTime<chrono::Utc>| v
            .to_rfc3339());

        param_values.push(Box::new(id.to_string()));
        let id_pos = param_values.len();
        let sql = format!(
            "UPDATE tasks SET {} WHERE id = ?{}",
            sets.join(", "),
            id_pos
        );

        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let rows = conn.execute(&sql, params_ref.as_slice())?;
        if rows == 0 {
            anyhow::bail!("Task not found: {id}");
        }
        Ok(())
    })?;

    get_task(config, id)
}

pub fn delete_task(config: &Config, id: &str) -> Result<()> {
    with_connection(config, |conn| {
        let rows = conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        if rows == 0 {
            anyhow::bail!("Task not found: {id}");
        }
        Ok(())
    })
}

// ── Comments ─────────────────────────────────────────────────

pub fn add_comment(
    config: &Config,
    task_id: &str,
    content: &str,
    author_type: ActorType,
    author_employee_id: Option<&str>,
    author_user_id: Option<&str>,
) -> Result<TaskComment> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO task_comments (id, task_id, content, author_type, author_employee_id, author_user_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                task_id,
                content,
                author_type.as_str(),
                author_employee_id,
                author_user_id,
                now.to_rfc3339(),
            ],
        )?;

        // Record comment event
        let event_id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO task_events (id, task_id, event_type, actor_type, actor_employee_id, actor_user_id, data, created_at)
             VALUES (?1, ?2, 'COMMENT', ?3, ?4, ?5, ?6, ?7)",
            params![
                event_id,
                task_id,
                author_type.as_str(),
                author_employee_id,
                author_user_id,
                serde_json::json!({"comment_id": id}).to_string(),
                now.to_rfc3339(),
            ],
        )?;

        Ok(())
    })?;

    get_comment(config, &id)
}

pub fn get_comment(config: &Config, id: &str) -> Result<TaskComment> {
    with_connection(config, |conn| {
        conn.prepare(
            "SELECT id, task_id, content, author_type, author_employee_id, author_user_id, created_at
             FROM task_comments WHERE id = ?1",
        )?
        .query_row(params![id], row_to_comment)
        .with_context(|| format!("Comment not found: {id}"))
    })
}

pub fn list_comments(config: &Config, task_id: &str) -> Result<Vec<TaskComment>> {
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, task_id, content, author_type, author_employee_id, author_user_id, created_at
             FROM task_comments WHERE task_id = ?1 ORDER BY created_at ASC",
        )?;
        let comments = stmt
            .query_map(params![task_id], row_to_comment)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(comments)
    })
}

// ── Events ───────────────────────────────────────────────────

pub fn record_event(
    config: &Config,
    task_id: &str,
    event_type: TaskEventType,
    actor_type: ActorType,
    actor_employee_id: Option<&str>,
    actor_user_id: Option<&str>,
    data: serde_json::Value,
) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO task_events (id, task_id, event_type, actor_type, actor_employee_id, actor_user_id, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                task_id,
                event_type.as_str(),
                actor_type.as_str(),
                actor_employee_id,
                actor_user_id,
                serde_json::to_string(&data)?,
                now.to_rfc3339(),
            ],
        )?;
        Ok(())
    })
}

pub fn list_events(config: &Config, task_id: &str) -> Result<Vec<TaskEvent>> {
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, task_id, event_type, actor_type, actor_employee_id, actor_user_id, data, created_at
             FROM task_events WHERE task_id = ?1 ORDER BY created_at ASC",
        )?;
        let events = stmt
            .query_map(params![task_id], row_to_event)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(events)
    })
}

// ── Task detail (composite) ──────────────────────────────────

pub fn get_task_detail(config: &Config, id: &str) -> Result<TaskDetail> {
    let task = get_task(config, id)?;
    let subtasks = list_tasks(
        config,
        &TaskFilter {
            parent_task_id: Some(id.to_string()),
            ..TaskFilter::default()
        },
    )?;
    let comments = list_comments(config, id)?;
    let events = list_events(config, id)?;

    Ok(TaskDetail {
        task,
        subtasks,
        comments,
        events,
    })
}

// ── Row mappers ──────────────────────────────────────────────

fn parse_rfc3339(s: &str) -> rusqlite::Result<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(e),
        ))
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let status_str: String = row.get(4)?;
    let priority_str: String = row.get(5)?;
    let review_status_str: Option<String> = row.get(10)?;
    let created_at_str: String = row.get(20)?;
    let updated_at_str: String = row.get(21)?;
    let due_date_str: Option<String> = row.get(15)?;
    let completed_at_str: Option<String> = row.get(16)?;
    let metadata_str: String = row.get(19)?;

    Ok(Task {
        id: row.get(0)?,
        organization_id: row.get(1)?,
        title: row.get(2)?,
        description: row.get(3)?,
        status: TaskStatus::try_from(status_str.as_str()).unwrap_or_default(),
        priority: TaskPriority::try_from(priority_str.as_str()).unwrap_or_default(),
        assignee_id: row.get(6)?,
        group_id: row.get(7)?,
        reviewer_id: row.get(8)?,
        human_review: row.get(9)?,
        review_status: review_status_str
            .and_then(|s| ReviewStatus::try_from(s.as_str()).ok()),
        review_comment: row.get(11)?,
        parent_task_id: row.get(12)?,
        created_by_employee_id: row.get(13)?,
        created_by_user_id: row.get(14)?,
        due_date: due_date_str.map(|s| parse_rfc3339(&s)).transpose()?,
        completed_at: completed_at_str.map(|s| parse_rfc3339(&s)).transpose()?,
        order_in_status: row.get(17)?,
        order_in_parent: row.get(18)?,
        metadata: serde_json::from_str(&metadata_str).unwrap_or(serde_json::json!({})),
        created_at: parse_rfc3339(&created_at_str)?,
        updated_at: parse_rfc3339(&updated_at_str)?,
    })
}

fn row_to_comment(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskComment> {
    let author_type_str: String = row.get(3)?;
    let created_at_str: String = row.get(6)?;
    Ok(TaskComment {
        id: row.get(0)?,
        task_id: row.get(1)?,
        content: row.get(2)?,
        author_type: ActorType::try_from(author_type_str.as_str()).unwrap_or(ActorType::Human),
        author_employee_id: row.get(4)?,
        author_user_id: row.get(5)?,
        created_at: parse_rfc3339(&created_at_str)?,
    })
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskEvent> {
    let event_type_str: String = row.get(2)?;
    let actor_type_str: String = row.get(3)?;
    let data_str: String = row.get(6)?;
    let created_at_str: String = row.get(7)?;
    Ok(TaskEvent {
        id: row.get(0)?,
        task_id: row.get(1)?,
        event_type: TaskEventType::try_from(event_type_str.as_str())
            .unwrap_or(TaskEventType::Created),
        actor_type: ActorType::try_from(actor_type_str.as_str()).unwrap_or(ActorType::Human),
        actor_employee_id: row.get(4)?,
        actor_user_id: row.get(5)?,
        data: serde_json::from_str(&data_str).unwrap_or(serde_json::json!({})),
        created_at: parse_rfc3339(&created_at_str)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    #[test]
    fn create_and_get_task() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let task = create_task(
            &config,
            &CreateTask {
                title: "Test task".into(),
                description: Some("A test".into()),
                priority: Some(TaskPriority::High),
                assignee_id: Some("emp-1".into()),
                group_id: None,
                reviewer_id: None,
                human_review: None,
                parent_task_id: None,
                due_date: None,
                organization_id: Some("org-1".into()),
                created_by_employee_id: Some("emp-2".into()),
                created_by_user_id: None,
                metadata: None,
            },
        )
        .unwrap();

        assert_eq!(task.title, "Test task");
        assert_eq!(task.status, TaskStatus::Todo);
        assert_eq!(task.priority, TaskPriority::High);
        assert_eq!(task.assignee_id.as_deref(), Some("emp-1"));

        let fetched = get_task(&config, &task.id).unwrap();
        assert_eq!(fetched.id, task.id);
    }

    #[test]
    fn list_tasks_with_filters() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        create_task(
            &config,
            &CreateTask {
                title: "Task A".into(),
                assignee_id: Some("emp-1".into()),
                ..default_create()
            },
        )
        .unwrap();
        create_task(
            &config,
            &CreateTask {
                title: "Task B".into(),
                assignee_id: Some("emp-2".into()),
                ..default_create()
            },
        )
        .unwrap();

        let all = list_tasks(&config, &TaskFilter::default()).unwrap();
        assert_eq!(all.len(), 2);

        let filtered = list_tasks(
            &config,
            &TaskFilter {
                assignee_id: Some("emp-1".into()),
                ..TaskFilter::default()
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "Task A");
    }

    #[test]
    fn update_task_fields() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let task = create_task(
            &config,
            &CreateTask {
                title: "Original".into(),
                ..default_create()
            },
        )
        .unwrap();

        let updated = update_task(
            &config,
            &task.id,
            &TaskPatch {
                title: Some("Updated".into()),
                priority: Some(TaskPriority::Low),
                ..TaskPatch::default()
            },
        )
        .unwrap();

        assert_eq!(updated.title, "Updated");
        assert_eq!(updated.priority, TaskPriority::Low);
    }

    #[test]
    fn delete_task_removes_it() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let task = create_task(
            &config,
            &CreateTask {
                title: "To delete".into(),
                ..default_create()
            },
        )
        .unwrap();

        delete_task(&config, &task.id).unwrap();
        assert!(get_task(&config, &task.id).is_err());
    }

    #[test]
    fn subtasks_linked_to_parent() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let parent = create_task(
            &config,
            &CreateTask {
                title: "Parent".into(),
                ..default_create()
            },
        )
        .unwrap();
        create_task(
            &config,
            &CreateTask {
                title: "Subtask 1".into(),
                parent_task_id: Some(parent.id.clone()),
                ..default_create()
            },
        )
        .unwrap();
        create_task(
            &config,
            &CreateTask {
                title: "Subtask 2".into(),
                parent_task_id: Some(parent.id.clone()),
                ..default_create()
            },
        )
        .unwrap();

        let detail = get_task_detail(&config, &parent.id).unwrap();
        assert_eq!(detail.subtasks.len(), 2);
    }

    #[test]
    fn comments_and_events() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let task = create_task(
            &config,
            &CreateTask {
                title: "Commentable".into(),
                ..default_create()
            },
        )
        .unwrap();

        add_comment(
            &config,
            &task.id,
            "First comment",
            ActorType::Human,
            None,
            Some("user-1"),
        )
        .unwrap();
        add_comment(
            &config,
            &task.id,
            "Agent reply",
            ActorType::Employee,
            Some("emp-1"),
            None,
        )
        .unwrap();

        let comments = list_comments(&config, &task.id).unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].content, "First comment");

        // Events: 1 creation + 2 comment events
        let events = list_events(&config, &task.id).unwrap();
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn delete_nonexistent_task_fails() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        assert!(delete_task(&config, "nonexistent").is_err());
    }

    fn default_create() -> CreateTask {
        CreateTask {
            title: String::new(),
            description: None,
            priority: None,
            assignee_id: None,
            group_id: None,
            reviewer_id: None,
            human_review: None,
            parent_task_id: None,
            due_date: None,
            organization_id: None,
            created_by_employee_id: None,
            created_by_user_id: None,
            metadata: None,
        }
    }
}

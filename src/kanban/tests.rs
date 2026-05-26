//! End-to-end tests that exercise the kanban module across submodules
//! (CLI write → DB read, slash parse, dispatcher tick, notifier delivery).

use crate::kanban::cli::{handle_command, CreateArgs, KanbanCommand, ListArgs};
use crate::kanban::store::connect;
use crate::kanban::test_env::with_temp_home;

#[test]
fn cli_create_then_list_roundtrip() {
    with_temp_home(|_| {
        let create = KanbanCommand::Create(CreateArgs {
            title: "research thing".into(),
            body: None,
            assignee: Some("researcher".into()),
            parents: vec![],
            tenant: None,
            workspace: "scratch".into(),
            priority: 1,
            triage: false,
            idempotency_key: None,
            max_runtime: None,
            skills: vec![],
            max_retries: None,
            json: false,
        });
        let mut buf = Vec::<u8>::new();
        handle_command(&create, None, &mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf).contains("Created "));

        let list = KanbanCommand::List(ListArgs {
            mine: false,
            assignee: None,
            status: None,
            tenant: None,
            archived: false,
            json: false,
        });
        let mut buf2 = Vec::<u8>::new();
        handle_command(&list, None, &mut buf2).unwrap();
        let s = String::from_utf8_lossy(&buf2);
        assert!(s.contains("research thing"));
    });
}

#[test]
fn boards_create_list_switch_roundtrip() {
    with_temp_home(|_| {
        let create = KanbanCommand::Boards {
            cmd: crate::kanban::cli::BoardsCommand::Create {
                slug: "atm10-server".into(),
                name: Some("ATM10".into()),
                description: None,
                icon: None,
                switch: true,
            },
        };
        let mut buf = Vec::<u8>::new();
        handle_command(&create, None, &mut buf).unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("created board atm10-server"));

        let list = KanbanCommand::Boards {
            cmd: crate::kanban::cli::BoardsCommand::List,
        };
        let mut buf2 = Vec::<u8>::new();
        handle_command(&list, None, &mut buf2).unwrap();
        assert!(String::from_utf8_lossy(&buf2).contains("atm10-server"));

        let show = KanbanCommand::Boards {
            cmd: crate::kanban::cli::BoardsCommand::Show,
        };
        let mut buf3 = Vec::<u8>::new();
        handle_command(&show, None, &mut buf3).unwrap();
        assert!(String::from_utf8_lossy(&buf3).contains("atm10-server"));
    });
}

#[test]
fn separate_boards_get_separate_dbs() {
    with_temp_home(|_| {
        crate::kanban::boards::create_board("alpha", None, None, None).unwrap();
        crate::kanban::boards::create_board("beta", None, None, None).unwrap();
        let alpha = connect(Some("alpha")).unwrap();
        let beta = connect(Some("beta")).unwrap();
        crate::kanban::store::create_task(
            &alpha,
            &crate::kanban::store::CreateTaskInput {
                title: "in alpha".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let alpha_tasks =
            crate::kanban::store::list_tasks(&alpha, &crate::kanban::store::ListFilter::default())
                .unwrap();
        let beta_tasks =
            crate::kanban::store::list_tasks(&beta, &crate::kanban::store::ListFilter::default())
                .unwrap();
        assert_eq!(alpha_tasks.len(), 1);
        assert_eq!(beta_tasks.len(), 0);
    });
}

#[test]
fn slash_parse_runs_handler() {
    with_temp_home(|_| {
        crate::kanban::store::init_db(None).unwrap();
        let out = crate::kanban::run_slash("list").unwrap();
        assert!(out.contains("(no tasks)"));
    });
}

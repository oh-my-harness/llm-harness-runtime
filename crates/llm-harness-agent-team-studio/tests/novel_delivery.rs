//! Controller regression with scripted messages and temporary non-story fixtures.
use llm_harness_agent_team::{AgentId, SocialContext, TaskMessageKind};
use llm_harness_agent_team_studio::{
    issue::{Issue, IssueStatus},
    novel_v1,
    pipeline::PipelineController,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

async fn scenario(valid: bool, direct_review: bool) {
    let workspace = tempfile::tempdir().unwrap();
    let path = "chapters/drafts/probe.txt";
    std::fs::create_dir_all(workspace.path().join("chapters/drafts")).unwrap();
    if valid {
        std::fs::write(workspace.path().join(path), b"delivery gate probe").unwrap();
    }
    let ctx = SocialContext::new(20);
    for id in ["orchestrator", "writer", "reviewer"] {
        ctx.register_agent(AgentId::new(id), id);
    }
    let controller = PipelineController::new(
        "NovelV1-test",
        ctx.clone(),
        workspace.path().into(),
        &["orchestrator".into(), "writer".into(), "reviewer".into()],
        3,
    );
    for id in ["orchestrator", "writer", "reviewer"] {
        ctx.ensure_contact(&AgentId::new(id), &AgentId::new("pipeline-controller"));
    }
    let reviews = Arc::new(AtomicUsize::new(0));
    let review_counter = reviews.clone();
    let actor = tokio::spawn(async move {
        let mut decisions = 0;
        loop {
            for role in ["orchestrator", "writer", "reviewer"] {
                for msg in ctx.drain_inbox(&AgentId::new(role)) {
                    let (body, kind) = match role {
                        "orchestrator" => {
                            decisions += 1;
                            let body = if decisions == 1 {
                                format!(
                                    "ACTION: DISPATCH {}\nreturn the probe evidence",
                                    if direct_review { "reviewer" } else { "writer" }
                                )
                            } else {
                                "ACTION: FINISH\ndry run finished".into()
                            };
                            (body, None)
                        }
                        "writer" => (
                            format!(
                                "NOVEL_ARTIFACTS: [{{\"path\":\"{path}\",\"sha256\":\"{}\"}}]",
                                novel_v1::hash(b"delivery gate probe")
                            ),
                            Some(TaskMessageKind::Result),
                        ),
                        _ => {
                            review_counter.fetch_add(1, Ordering::SeqCst);
                            assert!(msg.body.contains("delivered draft"));
                            assert!(msg.body.contains("Captured Snapshot files directory:"));
                            (
                                "read temporary probe; no findings".into(),
                                Some(TaskMessageKind::Result),
                            )
                        }
                    };
                    ctx.send_direct_with_task_kind(
                        &AgentId::new(role),
                        &AgentId::new("pipeline-controller"),
                        &body,
                        10000,
                        msg.task_id,
                        None,
                        kind,
                    )
                    .unwrap();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    });
    let issue = Issue {
        id: "delivery-test".into(),
        status: IssueStatus::Confirmed,
        source: "test".into(),
        created: "test".into(),
        title: "delivery gate fixture".into(),
        body: format!("NOVEL_DELIVERABLES: [\"{path}\"]"),
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), controller.run(&issue))
        .await
        .unwrap();
    actor.abort();
    if valid && !direct_review {
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(reviews.load(Ordering::SeqCst), 1);
    } else {
        assert!(result.is_err(), "{result:?}");
        assert_eq!(
            reviews.load(Ordering::SeqCst),
            0,
            "review must not start after failed delivery"
        );
    }
}
#[tokio::test]
async fn missing_delivery_never_enters_review() {
    scenario(false, false).await;
}
#[tokio::test]
async fn verified_delivery_enters_snapshot_review() {
    scenario(true, false).await;
}
#[tokio::test]
async fn orchestrator_cannot_bypass_delivery_by_dispatching_reviewer() {
    scenario(true, true).await;
}

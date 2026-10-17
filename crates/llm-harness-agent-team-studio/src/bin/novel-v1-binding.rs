//! Prepare a fresh Novel V1 namespace, or inspect actual assembled prompts.
//! Neither mode submits an Issue, sends a message or requests generation.
use std::{path::PathBuf, sync::Arc};
use llm_harness_agent_team_studio::{engine, novel_v1, team_spec::{TeamSpec, MemberSpec, ModelRef, TemplateRef, TemplateBaseline}};
use llm_harness_sandbox::OsEnv;
fn create_json(path: &std::path::Path, value: &impl serde::Serialize) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 || !matches!(args[0].as_str(), "prepare" | "dry-run") {
        return Err("usage: novel-v1-binding prepare DATA_ROOT WORKSPACE | dry-run DATA_ROOT TEAM_ID".into());
    }
    let root = PathBuf::from(&args[1]);
    if args[0] == "prepare" {
        let workspace = std::fs::canonicalize(&args[2])?;
        let members: Vec<_> = novel_v1::ROLES.iter().map(|(id, _)| Ok(MemberSpec {
            id: (*id).into(), persona: novel_v1::persona(&workspace, id)?, role_label: format!("Novel V1 {id}"),
            model: ModelRef::Tier(match *id { "orchestrator" => "strong", "writer" => "main", _ => "cheap" }.into()),
            toolkits: if *id == "orchestrator" { vec![] } else { vec!["fs".into()] }, behaviors: vec![], provider_override: None, persona_file: None,
        })).collect::<Result<_, std::io::Error>>()?;
        let spec = TeamSpec {
            id: "NovelV1-DaiYu".into(), name: "Novel V1 — 黛玉".into(), repo_path: Some(workspace.to_string_lossy().into()),
            template: Some(TemplateRef { template_id: novel_v1::TEMPLATE_ID.into(), version: "1".into() }),
            template_baseline: Some(TemplateBaseline { version: "1".into(), members: members.clone() }), sandbox_mode: None, members,
        };
        novel_v1::validate_binding(&spec)?;
        engine::validate_spec(&spec)?;
        let template = serde_json::json!({"id":"novel-v1","version":"1","name":"Novel V1","members":spec.members});
        let team_path = root.join("teams").join(&spec.id).join("team.json");
        let template_path = root.join("templates/novel-v1/team.json");
        if team_path.exists() || template_path.exists() { return Err("fresh namespace already exists; refusing overwrite".into()); }
        create_json(&template_path, &template)?;
        create_json(&team_path, &spec)?;
        println!("{}", serde_json::json!({"prepared_team":spec.id,"team_path":team_path,"template_path":template_path,"workspace":workspace,"writing_started":false}));
        return Ok(());
    }
    let team_path = root.join("teams").join(&args[2]).join("team.json");
    let spec: TeamSpec = serde_json::from_slice(&std::fs::read(&team_path)?)?;
    novel_v1::validate_binding(&spec)?;
    engine::validate_spec(&spec)?;
    let loaded = llm_harness_agent_team_studio::template_store::TemplateStore::new(Some(root.join("templates"))).load("novel-v1")?;
    if loaded.spec.members != spec.members { return Err("template/member binding mismatch".into()); }
    let workspace = PathBuf::from(spec.repo_path.as_ref().ok_or("missing workspace")?);
    let data = std::env::temp_dir().join(format!("novel-v1-assembly-dry-run-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&data)?;
    let mut settings = llm_harness_agent_team_studio::settings::Settings::default();
    settings.base_url = "http://127.0.0.1:1/v1".into();
    settings.models = llm_harness_agent_team_studio::roles::ModelTier { strong: "dry-run-unused".into(), main: "dry-run-unused".into(), cheap: "dry-run-unused".into() };
    let mut assembled = engine::assemble(engine::AssembleInputs {
        spec: spec.clone(), settings, api_key: "dry-run-unused".into(), env: Arc::new(OsEnv::new(workspace.clone())),
        data_dir: data.clone(), repo_path: Some(workspace.clone()),
    }).await?;
    let mut mapping = Vec::new();
    for (role, source_path) in novel_v1::ROLES {
        let m = spec.members.iter().find(|m| m.id == role).ok_or("missing role")?;
        let h = assembled.team.harness(&llm_harness_agent_team::AgentId::new(role)).ok_or("missing runtime harness")?;
        let prompt = h.state().system_prompt.as_deref().ok_or("missing system prompt")?.to_string();
        if !prompt.contains(&m.persona) || novel_v1::contains_coding_prompt(&prompt) { return Err(format!("actual persona mismatch for {role}").into()); }
        mapping.push(serde_json::json!({"role":role,"source":source_path,"source_sha256":novel_v1::hash(&std::fs::read(workspace.join(source_path))?),
            "persona_sha256":novel_v1::hash(m.persona.as_bytes()),"actual_system_prompt_sha256":novel_v1::hash(prompt.as_bytes()),
            "actual_loaded_persona":m.persona,"actual_system_prompt":prompt,"contains_coding_team_prompt":false}));
    }
    let controlled = assembled.workflow_gate.is_some();
    assembled.team.shutdown();
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "mode":"dry-run","team_id":spec.id,"loaded_by":"engine::assemble -> IndividualSpec::with_prompt -> actual harness state",
        "team_path":team_path,"workspace":workspace,"template":spec.template,"role_mapping":mapping,
        "contains_coding_team_prompt":false,"controlled_workflow":controlled,"isolated_dry_run_data":data,
        "issues_submitted":0,"messages_dispatched":0,"writing_started":false,"chapter_002_generated":false,
        "live_studio_hot_loaded":false
    }))?);
    Ok(())
}

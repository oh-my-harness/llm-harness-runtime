//! Novel V1 binding and mandatory draft-delivery checks. No model calls.
use std::{collections::BTreeSet, path::{Path, PathBuf}};
use sha2::{Digest, Sha256};
use crate::{team_spec::TeamSpec, pipeline::ArtifactSnapshot};
pub const TEMPLATE_ID: &str = "novel-v1";
pub const ROLES: [(&str, &str); 3] = [
    ("orchestrator", "architecture/roles/ORCHESTRATOR.md"),
    ("writer", "architecture/roles/WRITER.md"),
    ("reviewer", "architecture/roles/REVIEWER.md"),
];
pub fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
pub fn is_novel(spec: &TeamSpec) -> bool {
    spec.id == TEMPLATE_ID || spec.template.as_ref().is_some_and(|t| t.template_id == TEMPLATE_ID)
}
pub fn contains_coding_prompt(text: &str) -> bool {
    let s = text.to_lowercase();
    s.contains("coding team") || s.contains("coding-team") || s.contains("coding_team")
}
pub fn persona(workspace: &Path, role: &str) -> std::io::Result<String> {
    let (_, path) = ROLES.iter().find(|(id, _)| *id == role)
        .ok_or_else(|| std::io::Error::other("unknown Novel V1 role"))?;
    let source = std::fs::read_to_string(workspace.join(path))?;
    let boundary = match role {
        "orchestrator" => "你是 Novel V1 Orchestrator，只负责协调。只能转达作者目标与现有边界，不能新增字数要求、梦境频率、钩子要求、成长指标或章节公式，不写草稿、不修改任何小说文件。角色文件中复用 Coder 的历史说明在此绑定中无效；唯一写作成员为 writer。ACTION 第一行只能为 ACTION: DISPATCH <member_id>、ACTION: FINISH 或 ACTION: ESCALATE；DISPATCH 与 FINISH 须有非空正文。通过一次 send_message 向 pipeline-controller 回传精确 task_id，body 为唯一 ACTION 决策。不能自行绕过交付验证或直接派发 reviewer。",
        "writer" => "你是 Novel V1 Writer，成员 id 为 writer，只负责授权范围内生成或修订草稿。角色文件中复用 Coder、其他产物和状态差异落盘的旧说明在此绑定中无效；不写 canon、不发布、不接受、不协调、不写后续章节。完成后用一次 send_message 向 pipeline-controller 回传精确 task_id，task_kind 为 result、blocked 或 error，body 包含证据。result 必须含单行 NOVEL_ARTIFACTS: [{\"path\":\"授权草稿相对路径\",\"sha256\":\"实际文件SHA-256\"}]。路径必须与 Issue 的 NOVEL_DELIVERABLES 完全一致；不存在、空文件、哈希不符或 snapshot 未收录均禁止 review。",
        _ => "你是 Novel V1 Reviewer，只读取当前草稿 snapshot 和授权基线，提供原句、行号、基线依据与置信度的证据型评价。角色文件中要求 Coder 落盘的历史说明无效；不修改文件、不生成草稿、不协调、不接受、不发布。通过一次 send_message 向 pipeline-controller 回传精确 task_id，task_kind 为 result、blocked 或 error；result 只是任务完成，不代表 PASS。",
    };
    Ok(format!("# Novel V1 runtime binding（本轮作者要求优先）\n{boundary}\n所有角色禁止 Git commit、push 或历史改写。dry run 禁止写作；单章创作必须另有明确授权。\n\n# 来源：{path}\n{source}"))
}
pub fn validate_binding(spec: &TeamSpec) -> std::io::Result<()> {
    if !is_novel(spec) { return Ok(()); }
    let workspace = Path::new(spec.repo_path.as_deref().ok_or_else(|| std::io::Error::other("Novel V1 requires workspace"))?);
    let actual: BTreeSet<_> = spec.members.iter().map(|m| m.id.as_str()).collect();
    if actual != BTreeSet::from(["orchestrator", "writer", "reviewer"]) || spec.members.len() != 3 {
        return Err(std::io::Error::other("Novel V1 requires exactly orchestrator/writer/reviewer"));
    }
    for member in &spec.members {
        if member.persona != persona(workspace, &member.id)? || contains_coding_prompt(&member.persona)
            || member.persona_file.is_some() || !member.behaviors.is_empty()
            || member.toolkits.iter().any(|t| t != "fs") {
            return Err(std::io::Error::other(format!("Novel V1 binding mismatch for {}", member.id)));
        }
    }
    Ok(())
}
fn line_json<T: serde::de::DeserializeOwned>(text: &str, prefix: &str) -> Result<T, String> {
    let lines: Vec<_> = text.lines().filter_map(|l| l.strip_prefix(prefix)).collect();
    if lines.len() != 1 { return Err(format!("requires exactly one {prefix} line")); }
    serde_json::from_str(lines[0].trim()).map_err(|e| format!("invalid {prefix}: {e}"))
}
pub fn expected_paths(issue: &str) -> Result<Vec<String>, String> {
    let paths: Vec<String> = line_json(issue, "NOVEL_DELIVERABLES:")?;
    if paths.is_empty() || paths.iter().collect::<BTreeSet<_>>().len() != paths.len() {
        return Err("Novel deliverables must be a nonempty unique path list".into());
    }
    for path in &paths {
        let p = Path::new(path);
        if !path.starts_with("chapters/drafts/") || path.contains('\\')
            || p.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return Err(format!("unauthorized draft path: {path}"));
        }
    }
    Ok(paths)
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Delivery { path: String, sha256: String }
fn checked_file(root: &Path, rel: &str) -> Result<Vec<u8>, String> {
    let mut p: PathBuf = root.into();
    for c in Path::new(rel).components() {
        if !matches!(c, std::path::Component::Normal(_)) { return Err("unsafe delivery path".into()); }
        p.push(c);
        let meta = std::fs::symlink_metadata(&p).map_err(|e| format!("delivery missing: {}: {e}", p.display()))?;
        if meta.file_type().is_symlink() { return Err("delivery must not follow symlinks".into()); }
    }
    if !p.is_file() { return Err(format!("delivery is not a file: {rel}")); }
    let bytes = std::fs::read(p).map_err(|e| e.to_string())?;
    if bytes.is_empty() { return Err(format!("delivery is empty: {rel}")); }
    Ok(bytes)
}
pub fn verify_delivery(workspace: &Path, issue: &str, report: &str, snapshot: Option<&ArtifactSnapshot>) -> Result<(), String> {
    let expected = expected_paths(issue)?;
    let deliveries: Vec<Delivery> = line_json(report, "NOVEL_ARTIFACTS:")?;
    if deliveries.len() != expected.len() || deliveries.iter().map(|d| &d.path).collect::<BTreeSet<_>>() != expected.iter().collect() {
        return Err("reported artifacts differ from authorized deliverables".into());
    }
    for d in deliveries {
        let digest = hash(&checked_file(workspace, &d.path)?);
        if digest != d.sha256 { return Err(format!("delivery hash mismatch: {}", d.path)); }
        if let Some(s) = snapshot {
            if s.repo_root != workspace || !s.changed_files.contains(&d.path) || s.file_hashes.get(&d.path) != Some(&digest) {
                return Err(format!("delivery absent from snapshot or hash mismatch: {}", d.path));
            }
            if hash(&checked_file(&s.evidence_dir.join("files"), &d.path)?) != digest {
                return Err(format!("captured bytes mismatch: {}", d.path));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const PATH: &str = "chapters/drafts/delivery-probe.txt";
    fn setup() -> (tempfile::TempDir, String, String) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("chapters/drafts")).unwrap();
        std::fs::write(root.path().join(PATH), b"dry run delivery fixture").unwrap();
        let issue = format!("NOVEL_DELIVERABLES: [\"{PATH}\"]");
        let report = format!("NOVEL_ARTIFACTS: [{{\"path\":\"{PATH}\",\"sha256\":\"{}\"}}]", hash(b"dry run delivery fixture"));
        (root, issue, report)
    }
    #[test]
    fn binding_rejects_coding_persona_and_changed_role_source() {
        use crate::team_spec::{MemberSpec, ModelRef, TemplateRef};
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("architecture/roles")).unwrap();
        for (_, path) in ROLES { std::fs::write(root.path().join(path), "role fixture").unwrap(); }
        let members = ROLES.iter().map(|(id, _)| MemberSpec {
            id: (*id).into(), persona: persona(root.path(), id).unwrap(), role_label: (*id).into(),
            model: ModelRef::Tier("main".into()), toolkits: vec![], behaviors: vec![], provider_override: None, persona_file: None,
        }).collect();
        let mut spec = TeamSpec { id: "NovelV1-test".into(), name: "test".into(), repo_path: Some(root.path().to_string_lossy().into()),
            template: Some(TemplateRef { template_id: TEMPLATE_ID.into(), version: "1".into() }),
            template_baseline: None, sandbox_mode: None, members };
        assert!(validate_binding(&spec).is_ok());
        assert!(crate::pipeline::is_controlled(&spec));
        let original = spec.members[1].persona.clone();
        spec.members[1].persona = "Coding Team Coder".into();
        assert!(validate_binding(&spec).is_err());
        spec.members[1].persona = original;
        std::fs::write(root.path().join(ROLES[1].1), "changed source").unwrap();
        assert!(validate_binding(&spec).is_err());
    }
    #[test]
    fn delivery_requires_existing_nonempty_matching_bytes() {
        let (root, issue, report) = setup();
        assert!(verify_delivery(root.path(), &issue, &report, None).is_ok());
        std::fs::write(root.path().join(PATH), b"changed").unwrap();
        assert!(verify_delivery(root.path(), &issue, &report, None).unwrap_err().contains("hash mismatch"));
        std::fs::write(root.path().join(PATH), b"").unwrap();
        assert!(verify_delivery(root.path(), &issue, &report, None).unwrap_err().contains("empty"));
        std::fs::remove_file(root.path().join(PATH)).unwrap();
        assert!(verify_delivery(root.path(), &issue, &report, None).unwrap_err().contains("missing"));
    }
    #[test]
    fn delivery_requires_snapshot_membership_and_captured_hash() {
        let (root, issue, report) = setup();
        let mut snap = ArtifactSnapshot::collect(root.path(), "dry-run", uuid::Uuid::new_v4(), 1).unwrap();
        assert!(verify_delivery(root.path(), &issue, &report, Some(&snap)).is_ok());
        snap.changed_files.retain(|p| p != PATH);
        assert!(verify_delivery(root.path(), &issue, &report, Some(&snap)).is_err());
        snap.changed_files.push(PATH.into());
        snap.file_hashes.insert(PATH.into(), "wrong".into());
        assert!(verify_delivery(root.path(), &issue, &report, Some(&snap)).is_err());
        snap.file_hashes.insert(PATH.into(), hash(b"dry run delivery fixture"));
        std::fs::write(snap.evidence_dir.join("files").join(PATH), b"corrupt snapshot").unwrap();
        assert!(verify_delivery(root.path(), &issue, &report, Some(&snap)).is_err());
    }
    #[test]
    fn delivery_rejects_missing_duplicate_and_unauthorized_contracts() {
        assert!(expected_paths("no manifest").is_err());
        assert!(expected_paths("NOVEL_DELIVERABLES: []").is_err());
        assert!(expected_paths("NOVEL_DELIVERABLES: [\"canon/STORY_STATE.md\"]").is_err());
        assert!(expected_paths("NOVEL_DELIVERABLES: [\"chapters/drafts/../../canon/a\"]").is_err());
        let (root, issue, _) = setup();
        assert!(verify_delivery(root.path(), &issue, "completed", None).is_err());
        assert!(verify_delivery(root.path(), &issue, "NOVEL_ARTIFACTS: []", None).is_err());
        assert!(expected_paths(&format!("{issue}\n{issue}")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn delivery_rejects_symlink() {
        let (root, issue, report) = setup();
        std::fs::remove_file(root.path().join(PATH)).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", root.path().join(PATH)).unwrap();
        assert!(verify_delivery(root.path(), &issue, &report, None).unwrap_err().contains("symlinks"));
    }
}

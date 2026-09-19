import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "../api";
import {
  connectAgentTeamEvents,
  type AgentTeamEvent,
  type AgentTeamEventConnectionState,
} from "../agentTeamEvents";
import {
  buildMigrateDataCommand,
  buildRollbackDataCommand,
} from "../migration";
import type {
  AppDataMigrationInfo,
  AgentTeamProject,
  AgentTeamPulse,
  AgentTeamStartupRecovery,
  AgentTeamSettings,
  AgentTeamTemplate,
} from "../types";

const TEAM_ID_PATTERN = /^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$/;

const RECOVERY_REASONS: Record<string, string> = {
  invalid_spec: "配置无效",
  workspace_unauthorized: "工作区未授权",
  workspace_check: "工作区检查失败",
  team_create: "团队创建失败",
};

function agentStatusClass(status: string) {
  if (status === "busy") return "bg-amber-100 text-amber-800";
  if (status === "error" || status === "failed") return "bg-red-100 text-red-700";
  if (status === "idle" || status === "ready") return "bg-emerald-100 text-emerald-800";
  return "bg-gray-100 text-gray-700";
}

function eventSummary(event: AgentTeamEvent) {
  if (event.type === "operator_msg") {
    return `${event.from ?? "unknown"} → ${event.to ?? "unknown"}: ${event.text ?? ""}`;
  }
  if (event.type === "agent_error") {
    return `${event.agent ?? event.from ?? "unknown"}: ${event.text ?? "unknown error"}`;
  }
  if (event.type === "issue_changed") {
    return `issue changed: ${String(event.id ?? event.issue_id ?? "")}`;
  }
  return event.type;
}

export default function AgentTeamWorkspace() {
  const [teams, setTeams] = useState<AgentTeamProject[]>([]);
  const [templates, setTemplates] = useState<AgentTeamTemplate[]>([]);
  const [recovery, setRecovery] = useState<AgentTeamStartupRecovery | null>(null);
  const [settings, setSettings] = useState<AgentTeamSettings | null>(null);
  const [migrationInfo, setMigrationInfo] =
    useState<AppDataMigrationInfo | null>(null);
  const [migrationDestination, setMigrationDestination] = useState("");
  const [migrationBackup, setMigrationBackup] = useState("");
  const [settingsReady, setSettingsReady] = useState(false);
  const [selectedTeamId, setSelectedTeamId] = useState<string | null>(null);
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [pulse, setPulse] = useState<AgentTeamPulse | null>(null);
  const [events, setEvents] = useState<AgentTeamEvent[]>([]);
  const [connectionState, setConnectionState] =
    useState<AgentTeamEventConnectionState>("connecting");
  const [loadError, setLoadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [newTeamId, setNewTeamId] = useState("");
  const [newTeamName, setNewTeamName] = useState("");
  const [newTeamTemplate, setNewTeamTemplate] = useState("");
  const [newTeamRepo, setNewTeamRepo] = useState("");
  const [settingsStrongModel, setSettingsStrongModel] = useState("");
  const [settingsMainModel, setSettingsMainModel] = useState("");
  const [settingsCheapModel, setSettingsCheapModel] = useState("");
  const [settingsBaseUrl, setSettingsBaseUrl] = useState("");
  const [settingsScoutInterval, setSettingsScoutInterval] = useState("0");
  const [settingsApiKey, setSettingsApiKey] = useState("");
  const [messageText, setMessageText] = useState("");

  const selectedTeam = useMemo(
    () => teams.find((team) => team.id === selectedTeamId) ?? null,
    [teams, selectedTeamId]
  );

  const migrationCommands = useMemo(() => {
    if (!migrationInfo || !migrationDestination.trim()) return null;
    try {
      return {
        migrate: buildMigrateDataCommand({
          binary: migrationInfo.binary,
          source: migrationInfo.current_data_root,
          destination: migrationDestination.trim(),
          backup: migrationBackup.trim() || undefined,
        }),
        rollback: migrationBackup.trim()
          ? buildRollbackDataCommand({
              binary: migrationInfo.binary,
              source: migrationInfo.current_data_root,
              destination: migrationDestination.trim(),
              backup: migrationBackup.trim(),
            })
          : null,
        error: null,
      };
    } catch (error) {
      return {
        migrate: null,
        rollback: null,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }, [migrationBackup, migrationDestination, migrationInfo]);

  const loadTeams = useCallback(async () => {
    try {
      const [{ projects }, { templates }, { recovery }] = await Promise.all([
        api.listAgentTeams(),
        api.listAgentTeamTemplates(),
        api.getAgentTeamStartup(),
      ]);
      setTeams(projects);
      setTemplates(templates);
      setRecovery(recovery);
      setLoadError(null);
    } catch (error) {
      setLoadError(error instanceof Error ? error.message : String(error));
    } finally {
      setSettingsReady(true);
    }
  }, []);

  const loadSettings = useCallback(async () => {
    try {
      const loadedSettings = await api.getAgentTeamSettings();
      setSettings(loadedSettings);
      setSettingsStrongModel(loadedSettings.models.strong);
      setSettingsMainModel(loadedSettings.models.main);
      setSettingsCheapModel(loadedSettings.models.cheap);
      setSettingsBaseUrl(loadedSettings.base_url);
      setSettingsScoutInterval(String(loadedSettings.scout_interval_secs));
    } catch (error) {
      setLoadError(error instanceof Error ? error.message : String(error));
    }
  }, []);

  useEffect(() => {
    let cancelled = false;

    const loadInitialData = async () => {
      const [teamResult, settingsResult, migrationResult] =
        await Promise.allSettled([
        Promise.all([
          api.listAgentTeams(),
          api.listAgentTeamTemplates(),
          api.getAgentTeamStartup(),
        ]),
        api.getAgentTeamSettings(),
          api.getAppDataMigration(),
      ]);
      if (cancelled) return;

      if (teamResult.status === "fulfilled") {
        const [{ projects }, { templates }, { recovery }] = teamResult.value;
        setTeams(projects);
        setTemplates(templates);
        setRecovery(recovery);
        setLoadError(null);
      } else {
        setLoadError(
          teamResult.reason instanceof Error
            ? teamResult.reason.message
            : String(teamResult.reason)
        );
      }

      if (settingsResult.status === "fulfilled") {
        const loadedSettings = settingsResult.value;
        setSettings(loadedSettings);
        setSettingsStrongModel(loadedSettings.models.strong);
        setSettingsMainModel(loadedSettings.models.main);
        setSettingsCheapModel(loadedSettings.models.cheap);
        setSettingsBaseUrl(loadedSettings.base_url);
        setSettingsScoutInterval(String(loadedSettings.scout_interval_secs));
      } else {
        setLoadError(
          settingsResult.reason instanceof Error
            ? settingsResult.reason.message
            : String(settingsResult.reason)
        );
      }
      if (migrationResult.status === "fulfilled") {
        setMigrationInfo(migrationResult.value);
      } else {
        setLoadError(
          migrationResult.reason instanceof Error
            ? migrationResult.reason.message
            : String(migrationResult.reason)
        );
      }
      setSettingsReady(true);
    };

    void loadInitialData();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    const interval = window.setInterval(() => {
      void loadTeams();
    }, 15000);
    return () => window.clearInterval(interval);
  }, [loadTeams]);

  const loadPulse = useCallback(async (teamId: string) => {
    try {
      setPulse(await api.getAgentTeamPulse(teamId));
      setActionError(null);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : String(error));
    }
  }, []);

  useEffect(() => {
    if (!selectedTeamId) {
      return;
    }
    let cancelled = false;

    const fetchPulse = async () => {
      try {
        const loadedPulse = await api.getAgentTeamPulse(selectedTeamId);
        if (cancelled) return;
        setPulse(loadedPulse);
        setActionError(null);
      } catch (error) {
        if (cancelled) return;
        setActionError(error instanceof Error ? error.message : String(error));
      }
    };

    void fetchPulse();
    const interval = window.setInterval(() => {
      void fetchPulse();
    }, 5000);
    return () => {
      cancelled = true;
      window.clearInterval(interval);
    };
  }, [selectedTeamId]);

  useEffect(() => {
    const connection = connectAgentTeamEvents({
      onEvent: (event) => {
        setEvents((previous) => [event, ...previous].slice(0, 50));
        if (event.type === "issue_changed") {
          void loadTeams();
        }
      },
      onStateChange: setConnectionState,
    });
    return () => connection.close();
  }, [loadTeams]);

  const selectTeam = (teamId: string) => {
    setSelectedTeamId(teamId);
    setSelectedAgentId(null);
    setMessageText("");
    setPulse(null);
  };

  const createTeam = async () => {
    const id = newTeamId.trim();
    if (!TEAM_ID_PATTERN.test(id)) {
      setActionError("团队 ID 必须以字母或数字开头，只能包含字母、数字、下划线和连字符。");
      return;
    }
    setBusyAction("create");
    try {
      await api.createAgentTeam({
        id,
        name: newTeamName.trim() || undefined,
        template_id: newTeamTemplate || undefined,
        repo_path: newTeamRepo.trim() || undefined,
      });
      setNewTeamId("");
      setNewTeamName("");
      setNewTeamRepo("");
      await loadTeams();
      selectTeam(id);
      setActionError(null);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyAction(null);
    }
  };

  const saveSettings = async () => {
    if (!settingsStrongModel.trim() || !settingsMainModel.trim()) {
      setActionError("strong 和 main 模型 ID 不能为空。");
      return;
    }
    setBusyAction("settings");
    try {
      const scoutInterval = Number.parseInt(settingsScoutInterval, 10);
      if (!Number.isSafeInteger(scoutInterval) || scoutInterval < 0) {
        setActionError("Scout 间隔必须是非负整数。");
        return;
      }
      await api.updateAgentTeamSettings({
        strong: settingsStrongModel.trim(),
        main: settingsMainModel.trim(),
        cheap: settingsCheapModel.trim(),
        scout_interval_secs: scoutInterval,
        base_url: settingsBaseUrl.trim(),
        api_key: settingsApiKey.trim() || undefined,
      });
      setSettingsApiKey("");
      await loadSettings();
      setActionError(null);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyAction(null);
    }
  };

  const restartTeam = async (teamId: string) => {
    if (!window.confirm(`重启团队 ${teamId}？未完成的 inbox/timer 会恢复到一致状态。`)) return;
    setBusyAction(`restart:${teamId}`);
    try {
      await api.restartAgentTeam(teamId);
      await loadTeams();
      await loadPulse(teamId);
      setActionError(null);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyAction(null);
    }
  };

  const deleteTeam = async (teamId: string) => {
    if (!window.confirm(`删除团队 ${teamId}？团队配置将从运行时移除。`)) return;
    setBusyAction(`delete:${teamId}`);
    try {
      await api.deleteAgentTeam(teamId);
      await loadTeams();
      if (selectedTeamId === teamId) {
        setSelectedTeamId(null);
        setSelectedAgentId(null);
        setPulse(null);
      }
      setActionError(null);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyAction(null);
    }
  };

  const sendMessage = async () => {
    if (!selectedTeamId || !selectedAgentId || !messageText.trim()) return;
    setBusyAction("send");
    try {
      await api.sendAgentTeamMessage(
        selectedTeamId,
        selectedAgentId,
        messageText.trim()
      );
      setMessageText("");
      setActionError(null);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyAction(null);
    }
  };

  return (
    <div className="flex h-full min-h-0 flex-1 gap-4 p-4">
      <aside className="flex w-80 min-w-0 flex-col gap-3 overflow-y-auto rounded-xl border border-gray-200 bg-white p-4">
        <div>
          <h2 className="text-sm font-semibold text-gray-700">Agent Teams</h2>
          <p className="mt-1 text-xs text-gray-500">
            通过 runtime local API 管理团队；token 仅保存在当前标签页。
          </p>
        </div>
        <section
          className="rounded-lg border border-gray-200 p-3"
          data-testid="agent-team-runtime-settings"
          data-ready={settingsReady ? "true" : "false"}
        >
          <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-500">
            Runtime 设置
          </h3>
          <div className="mt-2 space-y-2">
            <input
              value={settingsStrongModel}
              onChange={(event) => setSettingsStrongModel(event.target.value)}
              placeholder="strong 模型"
              aria-label="AgentTeam strong 模型"
              disabled={!settingsReady || busyAction === "settings"}
              className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
            />
            <input
              value={settingsMainModel}
              onChange={(event) => setSettingsMainModel(event.target.value)}
              placeholder="main 模型"
              aria-label="AgentTeam main 模型"
              disabled={!settingsReady || busyAction === "settings"}
              className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
            />
            <input
              value={settingsCheapModel}
              onChange={(event) => setSettingsCheapModel(event.target.value)}
              placeholder="cheap 模型（可选）"
              aria-label="AgentTeam cheap 模型"
              disabled={!settingsReady || busyAction === "settings"}
              className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
            />
            <input
              value={settingsBaseUrl}
              onChange={(event) => setSettingsBaseUrl(event.target.value)}
              placeholder="API base URL（可选）"
              aria-label="AgentTeam API base URL"
              disabled={!settingsReady || busyAction === "settings"}
              className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
            />
            <input
              value={settingsScoutInterval}
              onChange={(event) => setSettingsScoutInterval(event.target.value)}
              placeholder="Scout 间隔秒数"
              aria-label="AgentTeam scout 间隔秒数"
              disabled={!settingsReady || busyAction === "settings"}
              inputMode="numeric"
              className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
            />
            <input
              value={settingsApiKey}
              onChange={(event) => setSettingsApiKey(event.target.value)}
              type="password"
              placeholder={
                settings?.api_key_set ? "已设置（输入替换）" : "API key"
              }
              aria-label="AgentTeam API key"
              disabled={!settingsReady || busyAction === "settings"}
              autoComplete="new-password"
              className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
            />
            <button
              onClick={saveSettings}
              disabled={!settingsReady || busyAction === "settings"}
              className="w-full rounded-lg border border-blue-500 px-3 py-2 text-sm font-medium text-blue-600 hover:bg-blue-50 disabled:opacity-50"
            >
              保存 Runtime 设置
            </button>
          </div>
        </section>
        <section
          className="rounded-lg border border-gray-200 p-3"
          data-testid="agent-team-data-migration"
        >
          <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-500">
            数据迁移
          </h3>
          <p
            className="mt-2 truncate text-xs text-gray-600"
            title={migrationInfo?.current_data_root ?? ""}
          >
            当前数据根：{migrationInfo?.current_data_root ?? "加载中"}
          </p>
          <p className="mt-2 text-xs text-amber-700">
            迁移和回滚必须在应用退出后执行；运行中的数据根会被锁拒绝。
          </p>
          <div className="mt-2 space-y-2">
            <input
              value={migrationDestination}
              onChange={(event) => setMigrationDestination(event.target.value)}
              placeholder="新数据根绝对路径"
              aria-label="数据迁移目标路径"
              className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
            />
            <input
              value={migrationBackup}
              onChange={(event) => setMigrationBackup(event.target.value)}
              placeholder="备份路径（可选）"
              aria-label="数据迁移备份路径"
              className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
            />
          </div>
          {migrationCommands?.error && (
            <p className="mt-2 text-xs text-red-600">{migrationCommands.error}</p>
          )}
          {migrationCommands?.migrate && (
            <div className="mt-3 space-y-2">
              <div>
                <p className="text-xs font-medium text-gray-600">迁移命令</p>
                <code className="mt-1 block break-all rounded-lg bg-gray-50 px-2 py-1 text-xs text-gray-700">
                  {migrationCommands.migrate}
                </code>
              </div>
              {migrationCommands.rollback && (
                <div>
                  <p className="text-xs font-medium text-gray-600">
                    回滚命令
                  </p>
                  <code className="mt-1 block break-all rounded-lg bg-gray-50 px-2 py-1 text-xs text-gray-700">
                    {migrationCommands.rollback}
                  </code>
                </div>
              )}
              <p className="text-xs text-gray-500">
                备份路径留空时会自动选择 sibling 备份；请记录命令输出中的
                backup 路径。迁移中断后重新执行同一条迁移命令可恢复事务。
              </p>
            </div>
          )}
        </section>
        <div className="space-y-2">
          <input
            value={newTeamId}
            onChange={(event) => setNewTeamId(event.target.value)}
            placeholder="团队 ID"
            aria-label="团队 ID"
            className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
          />
          <input
            value={newTeamName}
            onChange={(event) => setNewTeamName(event.target.value)}
            placeholder="显示名称（可选）"
            aria-label="团队显示名称"
            className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
          />
          <select
            value={newTeamTemplate}
            onChange={(event) => setNewTeamTemplate(event.target.value)}
            aria-label="团队模板"
            className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
          >
            <option value="">空白团队</option>
            {templates.map((template) => (
              <option key={`${template.id}:${template.version}`} value={template.id}>
                {template.name} ({template.id})
              </option>
            ))}
          </select>
          <input
            value={newTeamRepo}
            onChange={(event) => setNewTeamRepo(event.target.value)}
            placeholder="已授权工作区路径（可选）"
            aria-label="团队工作区路径"
            className="w-full rounded-lg border border-gray-300 px-3 py-2 text-sm"
          />
          <button
            onClick={createTeam}
            disabled={busyAction === "create"}
            className="w-full rounded-lg bg-blue-500 px-3 py-2 text-sm font-medium text-white hover:bg-blue-600 disabled:opacity-50"
          >
            创建团队
          </button>
        </div>
        <div className="space-y-2" data-testid="agent-team-list">
          {teams.length === 0 && (
            <p className="rounded-lg bg-gray-50 px-3 py-2 text-sm text-gray-500">暂无团队</p>
          )}
          {teams.map((team) => (
            <div
              key={team.id}
              className={`rounded-lg border p-3 ${
                team.id === selectedTeamId
                  ? "border-blue-400 bg-blue-50"
                  : "border-gray-200 hover:bg-gray-50"
              }`}
            >
              <button
                onClick={() => selectTeam(team.id)}
                className="w-full text-left text-sm font-medium text-gray-800"
              >
                {team.id}
              </button>
              <p className="mt-1 truncate text-xs text-gray-500" title={team.repo_path ?? ""}>
                {team.repo_path ?? "runtime 数据目录"}
              </p>
              <div className="mt-2 flex gap-2">
                <button
                  data-testid="agent-team-restart"
                  onClick={() => restartTeam(team.id)}
                  disabled={busyAction === `restart:${team.id}`}
                  className="rounded border border-gray-300 px-2 py-1 text-xs text-gray-700 hover:bg-gray-100 disabled:opacity-50"
                >
                  重启
                </button>
                <button
                  onClick={() => deleteTeam(team.id)}
                  disabled={busyAction === `delete:${team.id}`}
                  className="rounded border border-red-200 px-2 py-1 text-xs text-red-600 hover:bg-red-50 disabled:opacity-50"
                >
                  删除
                </button>
              </div>
            </div>
          ))}
        </div>
      </aside>

      <main className="flex min-w-0 flex-1 flex-col gap-4 overflow-hidden">
        {recovery?.status === "degraded" && (
          <div className="rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm text-amber-800">
            <strong>启动降级：</strong>已恢复 {recovery.restored_teams}/
            {recovery.persisted_teams} 个团队。
            {recovery.failed_teams.length > 0 && (
              <ul className="mt-2 list-disc pl-5">
                {recovery.failed_teams.map((failure) => (
                  <li key={`${failure.team_id}:${failure.reason}`}>
                    {failure.team_id}: {RECOVERY_REASONS[failure.reason] ?? failure.reason} —{" "}
                    {failure.detail}
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
        {(loadError || actionError) && (
          <div
            className="rounded-lg border border-red-300 bg-red-50 p-3 text-sm text-red-700"
            data-testid="agent-team-action-error"
            role="alert"
          >
            {loadError ?? actionError}
          </div>
        )}

        {!selectedTeam ? (
          <div className="flex flex-1 items-center justify-center rounded-xl border border-dashed border-gray-300 bg-white text-sm text-gray-500">
            选择或创建一个团队
          </div>
        ) : (
          <div className="grid min-h-0 flex-1 grid-cols-[minmax(240px,320px)_1fr] gap-4">
            <section className="min-h-0 overflow-y-auto rounded-xl border border-gray-200 bg-white p-4">
              <div className="flex items-center justify-between">
                <h2 className="text-sm font-semibold text-gray-700">成员</h2>
                <button
                  onClick={() => void loadPulse(selectedTeam.id)}
                  className="rounded border border-gray-300 px-2 py-1 text-xs text-gray-700 hover:bg-gray-100"
                >
                  刷新
                </button>
              </div>
              <div className="mt-3 space-y-2" data-testid="agent-team-members">
                {(pulse?.agents ?? []).map((agent) => (
                  <button
                    key={agent.id}
                    onClick={() => setSelectedAgentId(agent.id)}
                    className={`w-full rounded-lg border p-3 text-left ${
                      agent.id === selectedAgentId
                        ? "border-blue-400 bg-blue-50"
                        : "border-gray-200 hover:bg-gray-50"
                    }`}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="text-sm font-medium text-gray-800">{agent.id}</span>
                      <span
                        className={`rounded-full px-2 py-0.5 text-xs ${agentStatusClass(agent.status)}`}
                      >
                        {agent.status}
                      </span>
                    </div>
                    {(agent.phase || agent.activity?.task) && (
                      <p className="mt-1 truncate text-xs text-gray-500">
                        {[agent.phase, agent.activity?.task].filter(Boolean).join(" · ")}
                      </p>
                    )}
                    {agent.error && <p className="mt-1 text-xs text-red-600">{agent.error}</p>}
                  </button>
                ))}
                {pulse && pulse.agents.length === 0 && (
                  <p className="text-sm text-gray-500">团队没有成员</p>
                )}
              </div>
            </section>

            <section className="flex min-h-0 flex-col gap-4">
              <div className="grid grid-cols-3 gap-3">
                <div className="rounded-xl border border-gray-200 bg-white p-3">
                  <p className="text-xs text-gray-500">待处理消息</p>
                  <p className="mt-1 text-xl font-semibold text-gray-800">
                    {pulse?.pending.length ?? 0}
                  </p>
                </div>
                <div className="rounded-xl border border-gray-200 bg-white p-3">
                  <p className="text-xs text-gray-500">待确认 issue</p>
                  <p className="mt-1 text-xl font-semibold text-gray-800">
                    {pulse?.issues.pending ?? 0}
                  </p>
                </div>
                <div className="rounded-xl border border-gray-200 bg-white p-3">
                  <p className="text-xs text-gray-500">活跃定时器</p>
                  <p className="mt-1 text-xl font-semibold text-gray-800">
                    {pulse?.timers.pending ?? 0}
                  </p>
                </div>
              </div>

              <div className="min-h-0 flex-1 overflow-y-auto rounded-xl border border-gray-200 bg-white p-4">
                <div className="flex items-center justify-between">
                  <h2 className="text-sm font-semibold text-gray-700">事件流</h2>
                  <span className="text-xs text-gray-500" data-testid="agent-team-connection">
                    {connectionState === "open" ? "已连接" : connectionState}
                  </span>
                </div>
                <div className="mt-3 space-y-2" data-testid="agent-team-events">
                  {events.length === 0 && <p className="text-sm text-gray-500">暂无事件</p>}
                  {events.map((event, index) => (
                    <p
                      key={`${event.type}:${index}`}
                      className="rounded-lg bg-gray-50 px-3 py-2 text-sm text-gray-700"
                    >
                      {eventSummary(event)}
                    </p>
                  ))}
                </div>
              </div>

              <div className="rounded-xl border border-gray-200 bg-white p-4">
                <label className="text-xs font-medium text-gray-600" htmlFor="agent-team-message">
                  发送给 {selectedAgentId ?? "先选择成员"}
                </label>
                <div className="mt-2 flex gap-2">
                  <input
                    id="agent-team-message"
                    value={messageText}
                    onChange={(event) => setMessageText(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") void sendMessage();
                    }}
                    disabled={!selectedAgentId}
                    placeholder="输入任务或消息"
                    className="flex-1 rounded-lg border border-gray-300 px-3 py-2 text-sm disabled:bg-gray-100"
                  />
                  <button
                    onClick={sendMessage}
                    disabled={!selectedAgentId || busyAction === "send" || !messageText.trim()}
                    className="rounded-lg bg-blue-500 px-4 py-2 text-sm font-medium text-white hover:bg-blue-600 disabled:opacity-50"
                  >
                    发送
                  </button>
                </div>
              </div>
            </section>
          </div>
        )}
      </main>
    </div>
  );
}

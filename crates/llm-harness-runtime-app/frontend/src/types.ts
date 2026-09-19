export interface AppDataMigrationInfo {
  current_data_root: string;
  binary: string;
  requires_app_shutdown: boolean;
}

export interface AgentTeamProject {
  id: string;
  repo_path?: string | null;
}

export interface AgentTeamTemplate {
  id: string;
  version: string;
  name: string;
  member_ids: string[];
}

export interface AgentTeamSettings {
  models: {
    strong: string;
    main: string;
    cheap: string;
  };
  scout_interval_secs: number;
  base_url: string;
  api_key_set: boolean;
}

export interface AgentTeamAgent {
  id: string;
  status: string;
  phase?: string | null;
  error?: string | null;
  activity?: {
    task?: string | null;
    tools?: string[];
  } | null;
}

export interface AgentTeamPendingMessage {
  from?: string;
  to: string;
  type?: string;
  body?: string;
}

export interface AgentTeamPulse {
  agents: AgentTeamAgent[];
  pending: AgentTeamPendingMessage[];
  timers: { pending: number };
  issues: { pending: number; confirmed: number };
}

export interface AgentTeamRecoveryFailure {
  team_id: string;
  reason: string;
  detail: string;
}

export interface AgentTeamStartupRecovery {
  status: string;
  persisted_teams: number;
  restored_teams: number;
  failed_teams: AgentTeamRecoveryFailure[];
}

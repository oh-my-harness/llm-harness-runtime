import type {
  AgentTeamProject,
  AgentTeamPulse,
  AgentTeamSettings,
  AgentTeamStartupRecovery,
  AgentTeamTemplate,
} from "./types";

const API_BASE = "/api/agent-team";
const TOKEN_STORAGE_KEY = "runtime-app-token";

let cachedToken: string | null = null;

export function readPanelToken(): string {
  if (cachedToken !== null) return cachedToken;
  if (typeof location === "undefined") return "";

  if (location.hash.includes("token=")) {
    const token = new URLSearchParams(location.hash.slice(1)).get("token") ?? "";
    sessionStorage.setItem(TOKEN_STORAGE_KEY, token);
    history.replaceState(null, "", location.pathname + location.search);
    cachedToken = token;
    return token;
  }

  cachedToken = sessionStorage.getItem(TOKEN_STORAGE_KEY) ?? "";
  return cachedToken;
}

function authorizationHeaders(): Record<string, string> {
  const token = readPanelToken();
  return token ? { Authorization: `Bearer ${token}` } : {};
}

async function fetchJson<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    ...init,
    credentials: "same-origin",
    headers: {
      ...authorizationHeaders(),
      ...(init?.body ? { "Content-Type": "application/json" } : {}),
    },
  });
  if (!response.ok) {
    throw new Error(`${response.status}: ${await response.text()}`);
  }
  return (await response.json()) as T;
}

export const api = {
  listAgentTeams: () =>
    fetchJson<{ projects: AgentTeamProject[] }>(`${API_BASE}/projects`),
  createAgentTeam: (input: {
    id: string;
    name?: string;
    template_id?: string;
    repo_path?: string;
  }) =>
    fetchJson<{ ok: boolean }>(`${API_BASE}/projects`, {
      method: "POST",
      body: JSON.stringify(input),
    }),
  restartAgentTeam: (id: string) =>
    fetchJson<{ ok: boolean }>(
      `${API_BASE}/projects/restart?id=${encodeURIComponent(id)}`,
      { method: "POST" },
    ),
  deleteAgentTeam: (id: string) =>
    fetchJson<{ ok: boolean }>(
      `${API_BASE}/projects?id=${encodeURIComponent(id)}`,
      { method: "DELETE" },
    ),
  getAgentTeamPulse: (id: string) =>
    fetchJson<AgentTeamPulse>(
      `${API_BASE}/pulse?project=${encodeURIComponent(id)}`,
    ),
  sendAgentTeamMessage: (project: string, target: string, text: string) =>
    fetchJson<{ ok: boolean }>(`${API_BASE}/chat`, {
      method: "POST",
      body: JSON.stringify({ project, target, text }),
    }),
  listAgentTeamTemplates: () =>
    fetchJson<{ templates: AgentTeamTemplate[] }>(`${API_BASE}/templates`),
  getAgentTeamStartup: () =>
    fetchJson<{ recovery: AgentTeamStartupRecovery }>(`${API_BASE}/startup`),
  getAgentTeamSettings: () =>
    fetchJson<AgentTeamSettings>(`${API_BASE}/settings`),
  updateAgentTeamSettings: (input: {
    strong: string;
    main: string;
    cheap: string;
    scout_interval_secs: number;
    base_url: string;
    api_key?: string;
  }) =>
    fetchJson<{ ok: boolean; note: string }>(`${API_BASE}/settings`, {
      method: "POST",
      body: JSON.stringify(input),
    }),
};

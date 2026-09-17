import { beforeEach, describe, expect, it, vi } from "vitest";
import { readPanelToken } from "./api";

describe("runtime app API client", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.unstubAllGlobals();
  });

  it("reads the panel token from the URL and strips it from history", () => {
    const replaceState = vi.fn();
    const storage = new Map<string, string>();
    vi.stubGlobal("location", {
      hash: "#token=panel-token",
      pathname: "/app/agent-team",
      search: "",
    });
    vi.stubGlobal("history", { replaceState });
    vi.stubGlobal("sessionStorage", {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => storage.set(key, value),
    });

    expect(readPanelToken()).toBe("panel-token");
    expect(storage.get("runtime-app-token")).toBe("panel-token");
    expect(replaceState).toHaveBeenCalledWith(
      null,
      "",
      "/app/agent-team",
    );
  });

  it("uses the feature API prefix and bearer token", async () => {
    const storage = new Map<string, string>([["runtime-app-token", "stored-token"]]);
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ projects: [] }),
    });
    vi.stubGlobal("location", { hash: "", pathname: "/app/agent-team", search: "" });
    vi.stubGlobal("sessionStorage", {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => storage.set(key, value),
    });
    vi.stubGlobal("fetch", fetchMock);

    const { api } = await import("./api");
    await expect(api.listAgentTeams()).resolves.toEqual({ projects: [] });
    expect(fetchMock).toHaveBeenCalledWith("/api/agent-team/projects", {
      credentials: "same-origin",
      headers: { Authorization: "Bearer stored-token" },
    });
  });

  it("uses the feature API contract for every workspace operation", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ ok: true }),
    });
    vi.stubGlobal("location", { hash: "", pathname: "/app/agent-team", search: "" });
    vi.stubGlobal("sessionStorage", {
      getItem: () => "stored-token",
      setItem: vi.fn(),
    });
    vi.stubGlobal("fetch", fetchMock);

    const { api } = await import("./api");
    await api.createAgentTeam({
      id: "team",
      name: "Team",
      template_id: "coding-team",
      repo_path: "/repo",
    });
    await api.restartAgentTeam("team");
    await api.deleteAgentTeam("team");
    await api.getAgentTeamPulse("team");
    await api.sendAgentTeamMessage("team", "coder", "hello");
    await api.listAgentTeamTemplates();
    await api.getAgentTeamStartup();
    await api.getAgentTeamSettings();
    await api.updateAgentTeamSettings({
      strong: "strong-model",
      main: "main-model",
      cheap: "cheap-model",
      scout_interval_secs: 30,
      base_url: "http://127.0.0.1:8000",
      api_key: "secret",
    });

    expect(fetchMock).toHaveBeenNthCalledWith(
      1,
      "/api/agent-team/projects",
      expect.objectContaining({ method: "POST" })
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      2,
      "/api/agent-team/projects/restart?id=team",
      expect.objectContaining({ method: "POST" })
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      3,
      "/api/agent-team/projects?id=team",
      expect.objectContaining({ method: "DELETE" })
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      4,
      "/api/agent-team/pulse?project=team",
      expect.objectContaining({ credentials: "same-origin" })
    );
    expect(fetchMock.mock.calls[3]?.[1]).not.toHaveProperty("method");
    expect(fetchMock).toHaveBeenNthCalledWith(
      5,
      "/api/agent-team/chat",
      expect.objectContaining({ method: "POST" })
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      6,
      "/api/agent-team/templates",
      expect.objectContaining({ credentials: "same-origin" })
    );
    expect(fetchMock.mock.calls[5]?.[1]).not.toHaveProperty("method");
    expect(fetchMock).toHaveBeenNthCalledWith(
      7,
      "/api/agent-team/startup",
      expect.objectContaining({ credentials: "same-origin" })
    );
    expect(fetchMock.mock.calls[6]?.[1]).not.toHaveProperty("method");
    expect(fetchMock).toHaveBeenNthCalledWith(
      8,
      "/api/agent-team/settings",
      expect.objectContaining({ credentials: "same-origin" })
    );
    expect(fetchMock.mock.calls[7]?.[1]).not.toHaveProperty("method");
    expect(fetchMock).toHaveBeenNthCalledWith(
      9,
      "/api/agent-team/settings",
      expect.objectContaining({ method: "POST" })
    );
    expect(JSON.parse(String(fetchMock.mock.calls[0]?.[1]?.body))).toEqual({
      id: "team",
      name: "Team",
      template_id: "coding-team",
      repo_path: "/repo",
    });
    expect(JSON.parse(String(fetchMock.mock.calls[4]?.[1]?.body))).toEqual({
      project: "team",
      target: "coder",
      text: "hello",
    });
    expect(JSON.parse(String(fetchMock.mock.calls[8]?.[1]?.body))).toEqual({
      strong: "strong-model",
      main: "main-model",
      cheap: "cheap-model",
      scout_interval_secs: 30,
      base_url: "http://127.0.0.1:8000",
      api_key: "secret",
    });
  });
});

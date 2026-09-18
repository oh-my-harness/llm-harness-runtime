import { afterEach, describe, expect, it, vi } from "vitest";
import {
  connectAgentTeamEvents,
  createAgentTeamWebSocket,
  parseAgentTeamEvent,
} from "./agentTeamEvents";

class FakeWebSocket {
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;

  close() {
    this.onclose?.();
  }
}

describe("parseAgentTeamEvent", () => {
  it("accepts objects with a string type", () => {
    expect(parseAgentTeamEvent({ type: "operator_msg", text: "hello" })).toEqual({
      type: "operator_msg",
      text: "hello",
    });
  });

  it("rejects malformed events", () => {
    expect(parseAgentTeamEvent(null)).toBeNull();
    expect(parseAgentTeamEvent("operator_msg")).toBeNull();
    expect(parseAgentTeamEvent({ text: "missing type" })).toBeNull();
  });
});

describe("connectAgentTeamEvents", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("emits valid events and reconnects after the socket closes", () => {
    vi.useFakeTimers();
    vi.stubGlobal("window", globalThis);
    const sockets: FakeWebSocket[] = [];
    const states: string[] = [];
    const events: unknown[] = [];

    const connection = connectAgentTeamEvents({
      createSocket: () => {
        const socket = new FakeWebSocket();
        sockets.push(socket);
        return socket as unknown as WebSocket;
      },
      onEvent: (event) => events.push(event),
      onStateChange: (state) => states.push(state),
      reconnectDelayMs: 10,
    });

    expect(sockets).toHaveLength(1);
    sockets[0].onopen?.();
    sockets[0].onmessage?.({ data: JSON.stringify({ type: "operator_msg" }) });
    sockets[0].onmessage?.({ data: "not-json" });
    sockets[0].onmessage?.({ data: JSON.stringify({ text: "invalid" }) });
    sockets[0].close();

    expect(events).toEqual([{ type: "operator_msg" }]);
    expect(states).toEqual(["connecting", "open", "reconnecting"]);

    vi.advanceTimersByTime(10);
    expect(sockets).toHaveLength(2);
    sockets[1].onopen?.();
    expect(states).toEqual(["connecting", "open", "reconnecting", "connecting", "open"]);

    connection.close();
    vi.advanceTimersByTime(10);
    expect(sockets).toHaveLength(2);
  });
});

describe("connectAgentTeamEvents token handling", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("uses the panel token by default", () => {
    vi.resetModules();
    vi.stubGlobal("location", {
      hash: "#token=panel-token",
      pathname: "/app/agent-team",
      search: "",
    });
    vi.stubGlobal("history", { replaceState: vi.fn() });
    vi.stubGlobal("sessionStorage", {
      getItem: () => null,
      setItem: vi.fn(),
    });

    const tokens: string[] = [];
    class FakeWebSocket {
      close() {}
    }
    vi.stubGlobal("WebSocket", FakeWebSocket);

    const connection = connectAgentTeamEvents({
      createSocket: (token) => {
        tokens.push(token);
        return new FakeWebSocket() as unknown as WebSocket;
      },
      onEvent: vi.fn(),
    });

    expect(tokens).toEqual(["panel-token"]);
    connection.close();
  });
});

describe("createAgentTeamWebSocket", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("connects to the feature event stream with an encoded token", () => {
    const urls: string[] = [];
    class FakeWebSocket {
      constructor(url: string) {
        urls.push(url);
      }
    }

    vi.stubGlobal("location", {
      protocol: "http:",
      host: "127.0.0.1:7878",
    });
    vi.stubGlobal("WebSocket", FakeWebSocket);

    createAgentTeamWebSocket("token/value");
    expect(urls).toEqual([
      "ws://127.0.0.1:7878/api/agent-team/events?token=token%2Fvalue",
    ]);
  });
});

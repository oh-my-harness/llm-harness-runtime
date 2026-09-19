import { describe, expect, it } from "vitest";
import {
  buildMigrateDataCommand,
  buildRollbackDataCommand,
} from "./migration";

describe("data migration commands", () => {
  it("builds a migrate command with an optional backup", () => {
    expect(
      buildMigrateDataCommand({
        binary: "llm-harness-app",
        source: "/old/data",
        destination: "/new/data",
      })
    ).toBe('llm-harness-app migrate-data --from "/old/data" --to "/new/data"');

    expect(
      buildMigrateDataCommand({
        binary: "llm-harness-app",
        source: "C:/old data",
        destination: "C:/new data",
        backup: "C:/backup data",
      })
    ).toBe(
      'llm-harness-app migrate-data --from "C:/old data" --to "C:/new data" --backup "C:/backup data"'
    );
  });

  it("builds a rollback command", () => {
    expect(
      buildRollbackDataCommand({
        binary: "llm-harness-app",
        source: "/old/data",
        destination: "/new/data",
        backup: "/backup/data",
      })
    ).toBe(
      'llm-harness-app rollback-data --from "/old/data" --to "/new/data" --backup "/backup/data"'
    );
  });

  it("rejects unsafe path characters", () => {
    expect(() =>
      buildMigrateDataCommand({
        binary: "llm-harness-app",
        source: '/old "data"',
        destination: "/new/data",
      })
    ).toThrow(/Path cannot contain/);
  });
});

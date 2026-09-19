export interface AppDataMigrationInfo {
  current_data_root: string;
  binary: string;
  requires_app_shutdown: boolean;
}

function quotedPath(path: string): string {
  if (path.includes('"') || path.includes("\0")) {
    throw new Error("Path cannot contain double quotes or null bytes");
  }
  return `"${path}"`;
}

export function buildMigrateDataCommand(input: {
  binary: string;
  source: string;
  destination: string;
  backup?: string;
}): string {
  const command = [
    input.binary,
    "migrate-data",
    "--from",
    quotedPath(input.source),
    "--to",
    quotedPath(input.destination),
  ];
  if (input.backup) command.push("--backup", quotedPath(input.backup));
  return command.join(" ");
}

export function buildRollbackDataCommand(input: {
  binary: string;
  source: string;
  destination: string;
  backup: string;
}): string {
  return [
    input.binary,
    "rollback-data",
    "--from",
    quotedPath(input.source),
    "--to",
    quotedPath(input.destination),
    "--backup",
    quotedPath(input.backup),
  ].join(" ");
}

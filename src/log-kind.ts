import type { LogKind } from "./shell-events";

/**
 * stderr is not an error channel. Python's logging writes INFO there, and the
 * settings log used to paint every stderr line red.
 */
export function kindForProcessLine(stream: "out" | "err", line: string): LogKind {
  if (stream === "out") return "out";
  const level = line
    .trimStart()
    .match(/^(debug|info|warning|warn|error|critical|fatal)\b/i)?.[1]
    .toLowerCase();
  switch (level) {
    case "debug":
    case "info":
      return "out";
    case "warning":
    case "warn":
      return "warn";
    case "error":
    case "critical":
    case "fatal":
      return "err";
    default:
      return "err";
  }
}

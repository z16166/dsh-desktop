export type LogKind = "out" | "warn" | "err" | "sys";

export type LogEntry = {
  id: number;
  kind: LogKind;
  text: string;
};

export type StatusState = "starting" | "running" | "stopped" | "error";

export type StatusEvent = {
  state: StatusState;
  text: string;
};

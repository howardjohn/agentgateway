import { API_BASE_URL, post } from "./client";

export interface TimeRange {
  from?: string | null;
  to?: string | null;
}

export interface LogFilters {
  httpStatus?: number[];
  provider?: string[];
  requestModel?: string[];
  responseModel?: string[];
  traceId?: string | null;
  hasPayload?: boolean | null;
  attributes?: Record<string, unknown>;
}

export interface SearchLogsRequest {
  limit?: number;
  cursor?: string | null;
  timeRange?: TimeRange | null;
  filters?: LogFilters;
  includeAttributes?: boolean;
  includePayload?: boolean;
}

export interface GetLogRequest {
  id: string;
  includePayload?: boolean;
}

export interface TailLogsRequest {
  limit?: number;
  cursor?: string | null;
  filters?: LogFilters;
  includeAttributes?: boolean;
  includePayload?: boolean;
}

export interface GenAiEntry {
  operationName?: string | null;
  providerName?: string | null;
  requestModel?: string | null;
  responseModel?: string | null;
}

export interface UsageEntry {
  inputTokens?: number | null;
  outputTokens?: number | null;
  totalTokens?: number | null;
}

export interface PayloadEntry {
  requestPrompt?: unknown;
  responseCompletion?: unknown;
}

export interface LogEntry {
  id: string;
  startedAt: string;
  completedAt: string;
  durationMs: number;
  traceId?: string | null;
  spanId?: string | null;
  httpStatus?: number | null;
  error?: string | null;
  genAi: GenAiEntry;
  usage: UsageEntry;
  hasPayload: boolean;
  attributes?: unknown;
  payload?: PayloadEntry | null;
}

export interface SearchLogsResponse {
  logs: LogEntry[];
  nextCursor?: string | null;
}

export interface GetLogResponse {
  log?: LogEntry | null;
}

export interface TailLogEvent {
  entry: LogEntry;
  cursor: string;
}

export async function searchLogs(
  request: SearchLogsRequest,
): Promise<SearchLogsResponse> {
  return post<SearchLogsResponse>("/api/logs/search", {
    filters: {},
    ...request,
  });
}

export async function getLog(request: GetLogRequest): Promise<GetLogResponse> {
  return post<GetLogResponse>("/api/logs/get", request);
}

export async function tailLogs(
  request: TailLogsRequest,
  handlers: {
    onLog: (event: TailLogEvent) => void;
    onError?: (message: string) => void;
  },
  signal?: AbortSignal,
): Promise<void> {
  const response = await fetch(`${API_BASE_URL}/api/logs/tail`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      filters: {},
      ...request,
    }),
    signal,
  });

  if (!response.ok) {
    throw new Error((await response.text()) || "Failed to stream logs");
  }

  const reader = response.body?.getReader();
  if (!reader) {
    throw new Error("Log stream response did not include a body");
  }

  const decoder = new TextDecoder();
  let buffer = "";

  const processFrame = (frame: string) => {
    let eventName = "message";
    const data: string[] = [];

    for (const line of frame.split(/\r?\n/)) {
      if (line.startsWith("event:")) {
        eventName = line.slice("event:".length).trim();
      } else if (line.startsWith("data:")) {
        data.push(line.slice("data:".length).trimStart());
      }
    }

    if (eventName === "heartbeat" || data.length === 0) {
      return;
    }

    const payload = data.join("\n");
    if (eventName === "error") {
      try {
        const parsed = JSON.parse(payload) as { message?: string };
        handlers.onError?.(parsed.message || "Log stream failed");
      } catch {
        handlers.onError?.(payload || "Log stream failed");
      }
      return;
    }

    if (eventName === "log") {
      handlers.onLog(JSON.parse(payload) as TailLogEvent);
    }
  };

  while (true) {
    const { value, done } = await reader.read();
    if (done) {
      break;
    }

    buffer += decoder.decode(value, { stream: true });
    const frames = buffer.split(/\r?\n\r?\n/);
    buffer = frames.pop() ?? "";
    for (const frame of frames) {
      processFrame(frame);
    }
  }

  buffer += decoder.decode();
  if (buffer.trim()) {
    processFrame(buffer);
  }
}

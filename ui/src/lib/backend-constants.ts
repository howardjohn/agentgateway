export const DEFAULT_BACKEND_FORM = {
  name: "",
  weight: "1",
  // Route selection
  selectedBindPort: "",
  selectedListenerName: "",
  selectedRouteIndex: "",
  // Service backend fields
  serviceNamespace: "",
  serviceHostname: "",
  servicePort: "",
  // Host backend fields
  hostType: "address" as "address" | "hostname",
  hostAddress: "",
  hostHostname: "",
  hostPort: "",
  // MCP backend fields
  mcpTargets: [] as Array<{
    name: string;
    type: "sse" | "mcp" | "stdio" | "openapi";
    // SSE/MCP/OpenAPI fields
    host: string;
    port: string;
    path: string;
    // URL field for easier SSE/MCP/OpenAPI configuration
    fullUrl: string;
    // Stdio fields
    cmd: string;
    args: string[];
    env: Record<string, string>;
    // OpenAPI schema placeholder
    schema: boolean;
  }>,
  mcpStateful: true,
  // AI backend fields
  aiProvider: "openAI" as "openAI" | "gemini" | "vertex" | "anthropic" | "bedrock",
  aiModel: "",
  aiRegion: "",
  aiProjectId: "",
  aiHostOverrideType: "none" as "none" | "address" | "hostname",
  aiHostAddress: "",
  aiHostHostname: "",
  aiHostPort: "",
};

export const DEFAULT_MCP_TARGET = {
  name: "",
  type: "sse" as const,
  host: "",
  port: "",
  path: "",
  fullUrl: "",
  cmd: "",
  args: [] as string[],
  env: {} as Record<string, string>,
  schema: true,
};

export const BACKEND_TYPES = [
  { value: "mcp", label: "MCP", icon: "Target" },
  { value: "ai", label: "AI", icon: "Brain" },
  { value: "service", label: "Service", icon: "Cloud" },
  { value: "host", label: "Host", icon: "Server" },
  { value: "dynamic", label: "Dynamic", icon: "Globe" },
] as const;

export const BACKEND_TABLE_HEADERS = [
  "Name",
  "Type",
  "Listener",
  "Route",
  "Details",
  "Weight",
  "Actions",
] as const;

export const BACKEND_TYPE_COLORS = {
  mcp: "bg-blue-500 hover:bg-blue-600",
  ai: "bg-green-500 hover:bg-green-600",
  service: "bg-orange-500 hover:bg-orange-600",
  host: "bg-red-500 hover:bg-red-600",
  dynamic: "bg-yellow-500 hover:bg-yellow-600",
  default: "bg-gray-500 hover:bg-gray-600",
} as const;

export const HOST_TYPES = [
  { value: "address", label: "Direct Address" },
  { value: "hostname", label: "Hostname + Port" },
] as const;

export const AI_HOST_OVERRIDE_TYPES = [
  { value: "none", label: "None" },
  { value: "address", label: "Address" },
  { value: "hostname", label: "Hostname" },
] as const;

export const AI_MODEL_PLACEHOLDERS = {
  openAI: "gpt-4",
  gemini: "gemini-pro",
  vertex: "gemini-pro",
  anthropic: "claude-3-sonnet",
  bedrock: "anthropic.claude-3-sonnet",
} as const;

export const AI_REGION_PLACEHOLDERS = {
  vertex: "us-central1",
  bedrock: "us-east-1",
} as const;

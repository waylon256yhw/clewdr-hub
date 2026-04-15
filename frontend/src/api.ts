// --- Error ---

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

// --- Fetch wrapper ---

interface FetchOptions {
  method?: string;
  body?: unknown;
  params?: Record<string, string | number | undefined>;
}

export async function apiFetch<T>(path: string, opts?: FetchOptions): Promise<T> {
  const url = new URL(path, window.location.origin);
  if (opts?.params) {
    for (const [k, v] of Object.entries(opts.params)) {
      if (v !== undefined) url.searchParams.set(k, String(v));
    }
  }

  const headers: Record<string, string> = {};
  if (opts?.body) headers["Content-Type"] = "application/json";

  const res = await fetch(url.toString(), {
    method: opts?.method ?? "GET",
    headers,
    credentials: "include",
    body: opts?.body ? JSON.stringify(opts.body) : undefined,
  });

  if (res.status === 401) {
    window.dispatchEvent(new Event("auth:logout"));
    throw new ApiError(401, "登录已过期");
  }

  if (!res.ok) {
    let msg = res.statusText;
    try {
      const body = await res.json();
      msg = body?.error?.message ?? body?.error ?? msg;
    } catch {}
    throw new ApiError(res.status, msg);
  }

  if (res.status === 204) return undefined as T;
  return res.json();
}

// --- Types ---

export interface Paginated<T> {
  items: T[];
  total: number;
  offset: number;
  limit: number;
}

export interface OverviewResponse {
  version: string;
  server_time: string;
  pool: { valid: number; exhausted: number; invalid: number };
  users: { total: number; admins: number; members: number; disabled: number };
  api_keys: { total: number; active: number; disabled: number };
  accounts: {
    total: number;
    statuses: { active: number; cooling: number; error: number; disabled: number };
    auth_sources: { oauth: number; cookie: number; hybrid: number };
  };
  policies: number;
  requests_1h: number;
  requests_24h: number;
  stealth: { cli_version: string };
  must_change_password: boolean;
}

export interface UsageWindow {
  has_reset: boolean | null;
  resets_at: number | null;
  utilization: number | null;
}

export interface AccountRuntime {
  reset_time: number | null;
  resets_last_checked_at: number | null;
  session: UsageWindow | null;
  weekly: UsageWindow | null;
  weekly_sonnet: UsageWindow | null;
  weekly_opus: UsageWindow | null;
}

export interface Account {
  id: number;
  name: string;
  rr_order: number;
  drain_first: boolean;
  status: string;
  auth_source: "cookie" | "oauth" | "hybrid";
  has_cookie: boolean;
  has_oauth: boolean;
  oauth_expires_at: string | null;
  last_refresh_at: string | null;
  last_error: string | null;
  email: string | null;
  account_type: string | null;
  invalid_reason: string | null;
  created_at: string | null;
  updated_at: string | null;
  runtime: AccountRuntime | null;
}

export interface UserRow {
  id: number;
  username: string;
  display_name: string | null;
  role: string;
  policy_id: number;
  policy_name: string;
  disabled_at: string | null;
  last_seen_at: string | null;
  notes: string | null;
  key_count: number;
  current_week_cost_nanousd: number;
  current_month_cost_nanousd: number;
  created_at: string;
  updated_at: string;
}

export interface Policy {
  id: number;
  name: string;
  max_concurrent: number;
  rpm_limit: number;
  weekly_budget_nanousd: number;
  monthly_budget_nanousd: number;
  assigned_user_count: number;
  created_at: string;
  updated_at: string;
}

export interface KeyRow {
  id: number;
  user_id: number;
  username: string;
  label: string | null;
  lookup_key: string;
  plaintext_key: string | null;
  disabled_at: string | null;
  expires_at: string | null;
  last_used_at: string | null;
  last_used_ip: string | null;
  created_at: string;
  bound_account_ids: number[];
}

export interface KeyCreated {
  id: number;
  user_id: number;
  label: string | null;
  lookup_key: string;
  plaintext_key: string;
  created_at: string;
  bound_account_ids: number[];
}

export interface RequestLog {
  id: number;
  request_id: string;
  request_type: string;
  user_id: number | null;
  username: string | null;
  api_key_id: number | null;
  key_label: string | null;
  account_id: number | null;
  account_name: string | null;
  model_raw: string | null;
  model_normalized: string | null;
  stream: number;
  started_at: string;
  completed_at: string | null;
  duration_ms: number | null;
  ttft_ms: number | null;
  status: string;
  http_status: number | null;
  input_tokens: number | null;
  output_tokens: number | null;
  cache_creation_tokens: number | null;
  cache_read_tokens: number | null;
  cost_nanousd: number;
  error_code: string | null;
  error_message: string | null;
}

export interface LoginResponse {
  user_id: number;
  username: string;
  role: string;
  must_change_password: boolean;
}

export interface CliVersionsResponse {
  versions: string[];
  cached: boolean;
  fetched_at: string | null;
}

export interface OpsUsageTotals {
  request_count: number;
  input_tokens: number;
  output_tokens: number;
  cache_creation_tokens: number;
  cache_read_tokens: number;
  total_tokens: number;
  cost_nanousd: number;
}

export interface ModelDistributionItem {
  model: string;
  request_count: number;
  total_tokens: number;
  cost_nanousd: number;
}

export interface UserAggregate {
  user_id: number;
  username: string;
  request_count: number;
  total_tokens: number;
  cost_nanousd: number;
}

export interface UserSeriesPoint {
  bucket: string;
  request_count: number;
  total_tokens: number;
  cost_nanousd: number;
}

export interface UserSeries {
  user_id: number;
  username: string;
  points: UserSeriesPoint[];
}

export interface OpsUsageResponse {
  range: string;
  bucket_unit: "hour" | "day";
  selected_user_id: number | null;
  retention_days: number;
  coverage_limited: boolean;
  window_started_at: string;
  window_ended_at: string;
  buckets: string[];
  totals: OpsUsageTotals;
  model_distribution: ModelDistributionItem[];
  top_users: UserAggregate[];
  user_series: UserSeries[];
}

// --- Endpoints ---

// Auth
export const login = (data: { username: string; password: string }) =>
  apiFetch<LoginResponse>("/auth/login", { method: "POST", body: data });
export const logout = () =>
  apiFetch<void>("/auth/logout", { method: "POST" }).catch(() => {});

// Overview
export const getOverview = () => apiFetch<OverviewResponse>("/api/admin/overview");

export interface AccountsListResponse {
  items: Account[];
  total: number;
  offset: number;
  limit: number;
  probing_ids: number[];
  probe_errors: Record<string, string>;
}

// Accounts
export const listAccounts = () =>
  apiFetch<AccountsListResponse>("/api/admin/accounts", { params: { limit: 100 } });
export const createAccount = (data: {
  name: string;
  rr_order?: number;
  max_slots?: number;
  drain_first?: boolean;
  auth_source?: "cookie" | "oauth" | "hybrid";
  cookie_blob?: string;
  oauth_callback_input?: string;
  oauth_state?: string;
}) => apiFetch<Account>("/api/admin/accounts", { method: "POST", body: data });
export const updateAccount = (id: number, data: Record<string, unknown>) =>
  apiFetch<Account>(`/api/admin/accounts/${id}`, { method: "PUT", body: data });
export const deleteAccount = (id: number) =>
  apiFetch<void>(`/api/admin/accounts/${id}`, { method: "DELETE" });
export const probeAllAccounts = () =>
  apiFetch<{ probing_ids: number[] }>("/api/admin/accounts/probe", { method: "POST" });
export interface TestAccountResponse {
  success: boolean;
  latency_ms: number;
  error?: string;
  http_status?: number;
}
export const testAccount = (id: number) =>
  apiFetch<TestAccountResponse>(`/api/admin/accounts/${id}/test`, { method: "POST" });
export const startAccountOAuth = (data?: { redirect_uri?: string }) =>
  apiFetch<{ auth_url: string; state: string; redirect_uri: string }>(
    "/api/admin/accounts/oauth/start",
    { method: "POST", body: data ?? {} },
  );

// Users
export const listUsers = () =>
  apiFetch<Paginated<UserRow>>("/api/admin/users", { params: { limit: 100 } });
export const createUser = (data: Record<string, unknown>) =>
  apiFetch<UserRow>("/api/admin/users", { method: "POST", body: data });
export const updateUser = (id: number, data: Record<string, unknown>) =>
  apiFetch<UserRow>(`/api/admin/users/${id}`, { method: "PUT", body: data });
export const deleteUser = (id: number) =>
  apiFetch<void>(`/api/admin/users/${id}`, { method: "DELETE" });

// Policies
export const listPolicies = () =>
  apiFetch<Paginated<Policy>>("/api/admin/policies", { params: { limit: 100 } });
export const createPolicy = (data: Record<string, unknown>) =>
  apiFetch<Policy>("/api/admin/policies", { method: "POST", body: data });
export const updatePolicy = (id: number, data: Record<string, unknown>) =>
  apiFetch<Policy>(`/api/admin/policies/${id}`, { method: "PUT", body: data });
export const deletePolicy = (id: number) =>
  apiFetch<void>(`/api/admin/policies/${id}`, { method: "DELETE" });

// Keys
export const listKeys = (userId?: number) =>
  apiFetch<Paginated<KeyRow>>("/api/admin/keys", {
    params: { limit: 100, ...(userId !== undefined ? { user_id: userId } : {}) },
  });
export const createKey = (data: { user_id: number; label?: string; bound_account_ids?: number[] }) =>
  apiFetch<KeyCreated>("/api/admin/keys", { method: "POST", body: data });
export const deleteKey = (id: number) =>
  apiFetch<void>(`/api/admin/keys/${id}`, { method: "DELETE" });
export const updateKeyBindings = (id: number, accountIds: number[]) =>
  apiFetch<void>(`/api/admin/keys/${id}/bindings`, { method: "PUT", body: { account_ids: accountIds } });

// Settings
export const getSettings = () => apiFetch<Record<string, string>>("/api/admin/settings");
export const updateSettings = (settings: Record<string, string>) =>
  apiFetch<Record<string, string>>("/api/admin/settings", { method: "POST", body: { settings } });
export const getCliVersions = (force?: boolean) =>
  apiFetch<CliVersionsResponse>(`/api/admin/cli-versions${force ? "?force=1" : ""}`);

// Ops
export const getOpsUsage = (range: string, topUsers = 5, userId?: number) =>
  apiFetch<OpsUsageResponse>("/api/admin/ops/usage", {
    params: { range, top_users: topUsers, user_id: userId },
  });

// Requests
export interface RequestFilters {
  offset?: number;
  limit?: number;
  request_type?: string;
  user_id?: number;
  status?: string;
  model?: string;
  started_from?: string;
  started_to?: string;
}
export const listRequests = (filters: RequestFilters) =>
  apiFetch<Paginated<RequestLog>>("/api/admin/requests", {
    params: filters as Record<string, string | number | undefined>,
  });

export const getRequestResponseBody = (id: number) =>
  apiFetch<{ response_body: string | null }>(`/api/admin/requests/${id}/response_body`);

// Me
export const changePassword = (data: { current_password: string; new_password: string }) =>
  apiFetch<{ message: string }>("/api/admin/me/password", { method: "PUT", body: data });

// Models
export interface ModelRow {
  model_id: string;
  display_name: string;
  enabled: number;
  source: string;
  sort_order: number;
  created_at: string;
  updated_at: string;
}

export const listModelsAdmin = () =>
  apiFetch<Paginated<ModelRow>>("/api/admin/models", { params: { limit: 100 } });
export const createModel = (data: { model_id: string; display_name: string; sort_order?: number }) =>
  apiFetch<ModelRow>("/api/admin/models", { method: "POST", body: data });
export const updateModel = (modelId: string, data: { display_name?: string; enabled?: boolean; sort_order?: number }) =>
  apiFetch<ModelRow>(`/api/admin/models/${encodeURIComponent(modelId)}`, { method: "PUT", body: data });
export const deleteModel = (modelId: string) =>
  apiFetch<void>(`/api/admin/models/${encodeURIComponent(modelId)}`, { method: "DELETE" });
export const resetDefaultModels = () =>
  apiFetch<Paginated<ModelRow>>("/api/admin/models/reset-defaults", { method: "POST" });

// --- Query Keys ---

export const qk = {
  overview: ["overview"] as const,
  accounts: ["accounts"] as const,
  users: ["users"] as const,
  policies: ["policies"] as const,
  keys: (userId?: number) => ["keys", userId] as const,
  settings: ["settings"] as const,
  models: ["models"] as const,
  opsUsage: (range: string, topUsers: number, userId?: number) =>
    ["opsUsage", range, topUsers, userId] as const,
  requests: (filters: RequestFilters) => ["requests", filters] as const,
  requestBody: (id: number) => ["request_body", id] as const,
};

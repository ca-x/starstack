import { getLocale, t } from "./locale";
export interface Repo {
  id: number;
  full_name: string;
  description: string | null;
  language: string | null;
  topics: string[];
  stargazers_count: number;
  starred_at: string;
  html_url: string;
  archived: boolean;
  tags: string[];
  note: string;
  pinned: boolean;
  score?: number;
}

export interface Stats {
  total: number;
  languages: Record<string, number>;
  lists?: Record<string, number>;
  tags: Record<string, number>;
  last_sync: string | null;
  model: string;
}

export interface Filters {
  language?: string;
  tag?: string;
  list?: string;
  topic?: string;
  archived?: boolean;
  pinned?: boolean;
  since?: string;
  until?: string;
}

export type Sort = "time" | "stars";

export async function request(path: string, body?: object, signal?: AbortSignal) {
  const response = await fetch("/api/" + path, {
    method: body ? "POST" : "GET",
    headers: { "X-StarStack": "1", ...(body && { "Content-Type": "application/json" }) },
    credentials: "same-origin",
    body: body ? JSON.stringify(body) : undefined,
    signal,
  });
  if (!response.ok) {
    if (response.status === 401 && location.pathname !== "/login") location.replace("/login");
    const failure = await response.json().catch(() => ({}));
    throw new Error(failure.error ? t(failure.error) : t("请求失败（HTTP {0}）", response.status));
  }
  return response;
}

export async function api<T>(path: string, body?: object, signal?: AbortSignal): Promise<T> {
  return (await request(path, body, signal)).json();
}

export const message = (error: unknown) => error instanceof Error ? error.message : String(error);

export function counts(values: Record<string, number>) {
  return Object.entries(values).sort(([a, x], [b, y]) => y - x || a.localeCompare(b));
}

export function date(value: string | null, time = false) {
  if (!value) return t("尚未同步");
  const parsed = new Date(value);
  if (Number.isNaN(parsed.valueOf())) return value;
  return new Intl.DateTimeFormat(getLocale(), {
    year: "numeric", month: "2-digit", day: "2-digit",
    ...(time && { hour: "2-digit", minute: "2-digit" }),
  }).format(parsed);
}

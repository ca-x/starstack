import { getLocale, t } from "./locale";
import { useEffect, useRef, useState, type FormEvent } from "react";
import { EuiButton, EuiFieldNumber, EuiFormRow, EuiSelect, EuiSwitch } from "@elastic/eui";
import { api, message, request } from "./api";
import { Failure } from "./shared";

interface Quota {
  limit: number | null;
  remaining: number | null;
  reset_at: number | null;
  blocked_until: number | null;
}
type ReadmeSource = "github" | "firecrawl";
const retryTasks = [
  ["retry_sync", "收藏同步"], ["retry_readme", "README 补全"], ["retry_organize", "AI 自动整理"],
] as const;
type RetryKey = typeof retryTasks[number][0];
interface PendingRetry { next_at: number; attempts: number }
interface Settings extends Partial<Record<RetryKey, boolean>> {
  retry_pending?: { sync?: PendingRetry | null; readme?: PendingRetry | null };
  readme_source?: ReadmeSource;
  initial_readme_boost?: boolean;
  firecrawl_available?: boolean;
  sync_interval_minutes: number;
  quota_reserve_percent?: number;
  readme_budget?: number;
  readme_interval_minutes?: number;
  pending_sync?: string | null;
  quotas?: { rest?: Quota; graphql?: Quota };
  pending_readmes?: number;
}
const fields = [
  { key: "sync_interval_minutes", max: 10080, label: "同步间隔（分钟）", help: "填 0 关闭定期同步，最长可设为 10080 分钟。" },
  { key: "readme_interval_minutes", max: 10080, label: "README 后台补全间隔（分钟）", help: "单独控制后台补全任务。填 0 暂停，最长可设为 10080 分钟。" },
  { key: "quota_reserve_percent", max: 50, label: "额度预留比例（%）", help: "为其他 GitHub 工具留出部分额度。可设为 0 至 50%。" },
  { key: "readme_budget", max: 5000, label: "每轮 README 请求预算", help: "每轮最多用多少次请求补全 README。填 0 暂缓补全，最高 5000 次。" },
] as const;
type SettingKey = (typeof fields)[number]["key"];

function observedCount(value?: number | null) {
  return value != null && Number.isFinite(value) ? value.toLocaleString(getLocale()) : t("尚未观测");
}
function utcTime(seconds?: number | null) {
  if (seconds == null) return t("尚未观测");
  const instant = new Date(seconds * 1000);
  if (!Number.isFinite(instant.valueOf())) return t("尚未观测");
  return new Intl.DateTimeFormat(getLocale(), {
    timeZone: "UTC", year: "numeric", month: "short", day: "numeric",
    hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: "h23",
  }).format(instant) + " UTC";
}

function Quotas({ settings, jobActive, fetchReadmes }: { settings: Settings; jobActive: boolean; fetchReadmes: () => void }) {
  return <section className="operation">
    <div><h2>{t("GitHub API 额度（上次观测）")}</h2><p>{t("以下为 GitHub 上次返回的额度。其他工具可能已使用其中一部分。")}</p></div>
    <div className="quota-resources">{(["rest", "graphql"] as const).map(resource => {
      const quota = settings.quotas?.[resource];
      return <div key={resource} className="quota-resource"><h3>{resource === "rest" ? t("REST 请求") : t("GraphQL 点数")}</h3>
        <dl>
          <dt>{t("上次剩余")}</dt><dd>{observedCount(quota?.remaining)}</dd>
          <dt>{t("额度上限")}</dt><dd>{observedCount(quota?.limit)}</dd>
          <dt>{t("重置时间")}</dt><dd>{utcTime(quota?.reset_at)}</dd>
          <dt>{t("冷却截止")}</dt><dd>{utcTime(quota?.blocked_until)}</dd>
        </dl>
      </div>;
    })}</div>
    <dl className="inline-fact"><dt>{t("待补全 README 的仓库")}</dt><dd>{observedCount(settings.pending_readmes)}</dd></dl>
    <EuiButton isDisabled={jobActive} onClick={fetchReadmes}>{t("补全一批 README")}</EuiButton>
  </section>;
}

export function Schedule({ jobActive, confirm, fetchReadmes }: { jobActive: boolean; fetchReadmes: () => void; confirm: (id: string) => Promise<void> }) {
  const [settings, setSettings] = useState<Settings>();
  const [draft, setDraft] = useState<Partial<Record<SettingKey, string>>>({});
  const [retryDraft, setRetryDraft] = useState<Partial<Record<RetryKey, boolean>>>({});
  const [draftSource, setDraftSource] = useState<ReadmeSource>();
  const [draftBoost, setDraftBoost] = useState<boolean>();
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  const saveVersion = useRef(0);

  useEffect(() => {
    const controller = new AbortController();
    let timer: number;
    async function load(first: boolean) {
      const version = saveVersion.current;
      try {
        const data = await api<Settings>("settings", undefined, controller.signal);
        if (controller.signal.aborted) return;
        if (version === saveVersion.current) setSettings(data);
      } catch (e) {
        if (first && !controller.signal.aborted) setError(message(e));
      }
      if (!controller.signal.aborted) timer = window.setTimeout(() => load(false), 15000);
    }
    void load(true);
    return () => { controller.abort(); clearTimeout(timer); };
  }, [revision]);

  const baseline = {
    sync_interval_minutes: settings?.sync_interval_minutes,
    quota_reserve_percent: settings?.quota_reserve_percent ?? 5,
    readme_budget: settings?.readme_budget ?? 200,
    readme_interval_minutes: settings?.readme_interval_minutes ?? 30,
  };
  const source = settings?.readme_source ?? "github";
  const patch: Partial<Record<SettingKey, number>> & Partial<Record<RetryKey, boolean>> & Pick<Settings, "readme_source" | "initial_readme_boost"> = {};
  for (const [key] of retryTasks) {
    if (retryDraft[key] !== undefined && retryDraft[key] !== (settings?.[key] ?? true)) patch[key] = retryDraft[key];
  }
  if (draftSource && draftSource !== source) patch.readme_source = draftSource;
  if (draftBoost !== undefined && draftBoost !== (settings?.initial_readme_boost ?? false)) patch.initial_readme_boost = draftBoost;
  let valid = patch.readme_source !== "firecrawl" || settings?.firecrawl_available === true;
  for (const { key, max } of fields) {
    if (draft[key] === undefined) continue;
    const value = Number(draft[key]);
    if (!draft[key]?.trim() || !Number.isInteger(value) || value < 0 || value > max) valid = false;
    if (value !== baseline[key]) patch[key] = value;
  }
  const changed = Object.keys(patch).length > 0;
  async function save(e: FormEvent) {
    e.preventDefault();
    if (!settings || busy || !valid || !changed) return;
    saveVersion.current++;
    setBusy(true); setError(""); setSaved(false);
    try {
      await request("settings", patch);
      saveVersion.current++;
      setSettings(current => current && ({ ...current, ...patch }));
      setDraft({});
      setRetryDraft({});
      setDraftSource(undefined);
      setDraftBoost(undefined);
      setSaved(true);
      setRevision(n => n + 1);
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  }
  const pending = settings?.pending_sync;
  return <>{pending && <section className="operation"><div><h2>{t("AI 助手请求同步")}</h2><p>{t("确认后，StarStack 将从 GitHub 更新你的收藏。")}</p></div>
    <EuiButton fill isDisabled={jobActive} onClick={async () => { await confirm(pending); setRevision(n => n + 1); }}>{t("确认同步")}</EuiButton>
  </section>}
    {settings && <Quotas settings={settings} jobActive={jobActive} fetchReadmes={fetchReadmes} />}
    <section className="operation"><div><h2>{t("同步设置")}</h2><p>{t("按间隔刷新仓库元数据，核对已取消的 Stars，并更新向量索引。")}</p></div>
      <form onSubmit={save} className="schedule-form">
        <div className="schedule-fields">{fields.map(({ key, max, label, help }) =>
          <EuiFormRow key={key} fullWidth label={t(label)} helpText={t(help)}>
            <EuiFieldNumber fullWidth required min={0} max={max} step={1} value={draft[key] ?? (settings ? String(baseline[key]) : "")}
              disabled={!settings || busy} onChange={e => {
                const value = e.target.value;
                setDraft(current => {
                  const next = { ...current, [key]: value };
                  if (value !== "" && Number(value) === baseline[key]) delete next[key];
                  return next;
                });
                setSaved(false);
              }} />
          </EuiFormRow>
        )}
          <EuiFormRow fullWidth helpText={t("首次补全期间，按 GitHub 返回的可用额度将每轮预算提高至最多 1000 次。预算为 0 时暂停。")}>
            <EuiSwitch label={t("加速首次 README 补全")} checked={draftBoost ?? settings?.initial_readme_boost ?? false}
              disabled={!settings || busy} onChange={e => { setDraftBoost(e.target.checked); setSaved(false); }} />
          </EuiFormRow>
          <EuiFormRow fullWidth label={t("公开仓库 README 来源")} helpText={settings?.firecrawl_available
            ? t("Firecrawl Local 在本机抓取公开仓库 URL。私人仓库使用你的 GitHub 授权，收藏分类及成员始终来自 GitHub。")
            : t("需使用启用 Firecrawl Local 的构建。私人仓库、收藏分类及成员均通过 GitHub 获取。")}>
            <EuiSelect fullWidth value={draftSource ?? source} disabled={!settings || busy}
              options={[{ value: "github", text: "GitHub" }, { value: "firecrawl", text: t("Firecrawl Local（本机）"), disabled: !settings?.firecrawl_available }]}
              onChange={e => {
                const value = e.target.value as ReadmeSource;
                setDraftSource(value === source ? undefined : value); setSaved(false);
              }} />
          </EuiFormRow>
        </div>
        <fieldset className="retry-settings"><legend>{t("失败后自动重试")}</legend>
          <p>{t("临时失败会延迟重试，最多五次。GitHub 或模型服务要求等待更久时，会遵循服务端的等待时间。")}</p>
          <div className="row-controls">{retryTasks.map(([key,label]) => <EuiSwitch key={key} label={t(label)}
            checked={retryDraft[key] ?? settings?.[key] ?? true} disabled={!settings || busy}
            onChange={event => { setRetryDraft(draft => ({ ...draft, [key]: event.target.checked })); setSaved(false); }} />)}</div>
          {(["sync", "readme"] as const).map(task => {
            const next = settings?.retry_pending?.[task];
            return next && <p key={task} role="status">{t("{0}将于 {1} 之后自动重试。", t(task === "sync" ? "收藏同步" : "README 补全"), utcTime(next.next_at))}</p>;
          })}
        </fieldset>
        <EuiButton type="submit" isDisabled={!settings || !changed || !valid} isLoading={busy}>{t("保存设置")}</EuiButton>
      </form>
      {error && <Failure error={error} retry={!settings ? () => { setError(""); setRevision(n => n + 1); } : undefined} />}
      {saved && <p role="status">{t("设置已保存。")}</p>}
    </section></>;
}

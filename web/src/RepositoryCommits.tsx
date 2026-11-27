import { useEffect, useRef, useState } from "react";
import {
  EuiAccordion, EuiButton, EuiCallOut, EuiFieldNumber, EuiFormRow, EuiLink, EuiSwitch,
} from "@elastic/eui";
import { api, date, message } from "./api";
import { t } from "./locale";
import { Failure } from "./shared";

interface Commit { sha: string; committed_at: string; message: string }
interface State {
  enabled: boolean; limit: number; baseline: string | null; incomplete: boolean; total: number;
  entries: Commit[]; warning?: string;
}

export function RepositoryCommits({ id, url }: { id: number; url?: string }) {
  const [open, setOpen] = useState(false);
  const [state, setState] = useState<State>();
  const [offset, setOffset] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [limit, setLimit] = useState("");
  const [enabled, setEnabled] = useState(false);
  const [revision, setRevision] = useState(0);
  const request = useRef<AbortController>();
  useEffect(() => () => request.current?.abort(), []);
  useEffect(() => {
    if (!open || state) return;
    const controller = new AbortController();
    api<State>("repository/commits", { id }, controller.signal).then(data => {
      if (!controller.signal.aborted) { setState(data); setLimit(String(data.limit)); setEnabled(data.enabled); }
    }).catch(e => { if (!controller.signal.aborted) setError(message(e)); });
    return () => controller.abort();
  }, [open, state, id, revision]);

  const valid = /^\d+$/.test(limit) && +limit <= 10000;
  const changed = !!state && (enabled !== state.enabled || +limit !== state.limit);
  async function update(action: "save" | "refresh" | number) {
    if (busy || !state || action === "save" && !valid) return;
    const controller = new AbortController();
    request.current = controller;
    setBusy(true); setError("");
    try {
      if (action === "save") {
        const data = await api<State>("repository/commits", { id, enabled, limit: +limit }, controller.signal);
        setState(data); setOffset(0); setLimit(String(data.limit));
      } else if (action === "refresh") {
        setOffset(0);
        let data;
        do {
          data = await api<State>("repository/commits", { id, refresh: true }, controller.signal);
          setState(data);
        } while (data.incomplete && !controller.signal.aborted);
      } else {
        setState(await api<State>("repository/commits", { id, offset: action }, controller.signal));
        setOffset(action);
      }
    } catch (e) {
      if (!controller.signal.aborted) {
        setError(message(e));
        if (action === "refresh") {
          try { setState(await api<State>("repository/commits", { id }, controller.signal)); } catch { /* Keep the last observed page. */ }
        }
      }
    } finally { if (!controller.signal.aborted) setBusy(false); }
  }

  return <EuiAccordion className="repository-commit-panel" id={`repo-${id}-commits`} buttonContent={t("提交记录与基线")} paddingSize="m" onToggle={setOpen}>
    <div className="repository-assistant">
      <p>{t("按需记录默认分支的提交，用于回归定位。刷新保留已记录的旧提交，并核对新提交与旧基线的关系。")}</p>
      {error && <Failure error={error} retry={!state ? () => { setError(""); setRevision(n => n + 1); } : undefined} />}
      {!state && !error && <p role="status">{t("正在读取提交记录…")}</p>}
      {state && <>
        <EuiSwitch label={t("记录这个仓库的提交")} checked={enabled} disabled={busy} onChange={e => setEnabled(e.target.checked)} />
        {enabled && <EuiFormRow label={t("每次抓取的提交条数")} helpText={t("填 0 抓取全部可用历史。已记录的提交会保留。")} fullWidth>
          <EuiFieldNumber min={0} max={10000} step={1} value={limit} fullWidth disabled={busy} onChange={e => setLimit(e.target.value)} />
        </EuiFormRow>}
        <div className="row-controls">
          <EuiButton size="s" isDisabled={!changed || !valid || busy} onClick={() => update("save")}>{t("保存设置")}</EuiButton>
          {state.enabled && <EuiButton size="s" fill isLoading={busy} isDisabled={busy || changed} onClick={() => update("refresh")}>
            {t(state.incomplete ? "继续记录" : "刷新提交记录")}</EuiButton>}
        </div>
        <p role="status">{t("已记录 {0} 条提交", state.total.toLocaleString())}
          {state.baseline && <> · {t("最近基线")} <code title={state.baseline}>{state.baseline.slice(0, 10)}</code></>}</p>
        {state.warning && <EuiCallOut size="s" color="warning" title={t("提交基线需要核对")}><p>{state.warning}</p></EuiCallOut>}
        {!!state.entries.length && <ul className="repo-commit-list">{state.entries.map(commit => <li key={commit.sha}>
          {url ? <EuiLink href={url.replace(/\/$/, "") + "/commit/" + commit.sha} target="_blank" rel="noreferrer">
            <code>{commit.sha.slice(0, 10)}</code></EuiLink> : <code>{commit.sha.slice(0, 10)}</code>}
          <small>{date(commit.committed_at, true)}</small><p>{commit.message}</p>
        </li>)}</ul>}
        {state.total > 25 && <div className="row-controls">
          <EuiButton size="s" isDisabled={busy || offset === 0} onClick={() => update(Math.max(0, offset - 25))}>{t("上一页")}</EuiButton>
          <EuiButton size="s" isDisabled={busy || offset + 25 >= state.total} onClick={() => update(offset + 25)}>{t("下一页")}</EuiButton>
        </div>}
      </>}
    </div>
  </EuiAccordion>;
}

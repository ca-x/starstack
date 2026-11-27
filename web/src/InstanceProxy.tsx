import { useEffect, useState, type FormEvent } from "react";
import { EuiButton, EuiFieldPassword, EuiFormRow, EuiSelect } from "@elastic/eui";
import { api, message } from "./api";
import { t } from "./locale";
import { Failure } from "./shared";

type Mode = "inherit" | "system" | "direct" | "custom";
interface Settings {
  mode: Mode;
  source: "system" | "database" | "environment" | "instance";
  address: string | null;
  has_credentials: boolean;
}

function ProxySettings({ personal }: { personal: boolean }) {
  const endpoint = personal ? "proxy/account" : "proxy";
  const [current, setCurrent] = useState<Settings>();
  const [mode, setMode] = useState<Mode>(personal ? "inherit" : "system");
  const [url, setUrl] = useState("");
  const [revision, setRevision] = useState(0);
  const [busy, setBusy] = useState(false);
  const [testing, setTesting] = useState(false);
  const [latency, setLatency] = useState<number>();
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    const controller = new AbortController();
    setError("");
    api<Settings>(endpoint, undefined, controller.signal).then(result => {
      if (!controller.signal.aborted) { setCurrent(result); setMode(result.mode); }
    }).catch(e => { if (!controller.signal.aborted) setError(message(e)); });
    return () => controller.abort();
  }, [endpoint, revision]);
  const operator = current?.source === "environment";
  const changed = mode === "custom" ? !!url.trim() : current?.mode !== mode;
  const canTest = current && !testing && !busy && (mode !== "custom" || current.mode === "custom" || !!url.trim());
  async function test() {
    if (!canTest) return;
    setTesting(true); setError(""); setLatency(undefined);
    const candidate = operator || !changed ? null : mode === "custom" ? { mode, url: url.trim() } : { mode };
    try {
      const result = await api<{ elapsed_ms: number }>(`${endpoint}/test`, { candidate });
      setLatency(result.elapsed_ms);
    } catch (e) { setError(message(e)); } finally { setTesting(false); }
  }
  async function save(event: FormEvent) {
    event.preventDefault();
    if (!current || operator || busy || testing || !changed) return;
    setError(""); setSaved(false); setBusy(true);
    try {
      const result = await api<Settings>(endpoint, mode === "custom" ? { mode, url: url.trim() } : { mode });
      setCurrent(result); setMode(result.mode); setUrl(""); setSaved(true);
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  }
  return <section className="operations proxy-panel"><div className="operation">
    <div><h2>{t(personal ? "个人代理" : "实例默认代理")}</h2><p>{t(personal ? "用于你自己的 GitHub 授权、收藏同步和 README 抓取。" : "供继承默认设置的账号和实例模型下载使用。")}</p></div>
    {current?.address && <dl className="inline-fact"><dt>{t("当前代理")}</dt><dd>{current.address} {current.has_credentials && t("（已配置认证）")}</dd></dl>}
    {operator && <p>{t("此代理由部署环境中的 STARSTACK_PROXY_URL 管理。")}</p>}
    <form className="proxy-form" onSubmit={save}>
      <EuiFormRow label={t("连接方式")} fullWidth><EuiSelect fullWidth value={mode} disabled={!current || operator || busy || testing}
        options={[...(personal ? [{ value: "inherit", text: t("使用实例默认代理") }] : []), { value: "system", text: t("跟随系统代理") }, { value: "direct", text: t("直接连接") }, { value: "custom", text: t("指定代理") }]}
        onChange={e => { setMode(e.target.value as Mode); setUrl(""); setSaved(false); setLatency(undefined); setError(""); }} /></EuiFormRow>
      {mode === "custom" && !operator && <EuiFormRow label={t("新代理 URL")} fullWidth
        helpText={t("支持 HTTP、HTTPS、SOCKS5 和 SOCKS5h。替换地址时请同时填入所需的认证信息。")}>
        <EuiFieldPassword fullWidth type="dual" value={url} disabled={!current || busy || testing} autoComplete="new-password"
          placeholder="socks5h://proxy.example:1080" onChange={e => { setUrl(e.target.value); setSaved(false); setLatency(undefined); setError(""); }} />
      </EuiFormRow>}
      <div className="row-controls">
        <EuiButton type="button" isLoading={testing} isDisabled={!canTest} onClick={test}>{t("测试连接")}</EuiButton>
        {!operator && <EuiButton type="submit" fill isLoading={busy} isDisabled={!current || !changed || testing}>{t("保存代理")}</EuiButton>}
      </div>
    </form>
    {error && <Failure error={error} retry={!current ? () => setRevision(n => n + 1) : undefined} />}
    {latency !== undefined && <p role="status">{t("Cloudflare 连接成功，耗时 {0} 毫秒。", latency)}</p>}
    {saved && <p role="status">{t("代理设置已保存。")}</p>}
  </div></section>;
}

export function Connections({ admin }: { admin: boolean }) {
  return <>
    <div className="page-heading"><div><h1>{t("连接设置")}</h1></div></div>
    <div className="connection-panels">
      <ProxySettings personal />
      {admin && <ProxySettings personal={false} />}
    </div>
  </>;
}

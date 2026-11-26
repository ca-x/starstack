import { useEffect, useState, type FormEvent } from "react";
import { EuiButton, EuiFieldPassword, EuiFormRow, EuiSelect } from "@elastic/eui";
import { api, message } from "./api";
import { t } from "./locale";
import { Failure } from "./shared";

type Mode = "system" | "direct" | "custom";
interface Settings {
  mode: Mode;
  source: "system" | "database" | "environment";
  address: string | null;
  has_credentials: boolean;
}

export function InstanceProxy() {
  const [current, setCurrent] = useState<Settings>();
  const [mode, setMode] = useState<Mode>("system");
  const [url, setUrl] = useState("");
  const [revision, setRevision] = useState(0);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    const controller = new AbortController();
    setError("");
    api<Settings>("proxy", undefined, controller.signal).then(result => {
      if (!controller.signal.aborted) { setCurrent(result); setMode(result.mode); }
    }).catch(e => { if (!controller.signal.aborted) setError(message(e)); });
    return () => controller.abort();
  }, [revision]);
  const operator = current?.source === "environment";
  const changed = mode === "custom" ? !!url.trim() : current?.mode !== mode;
  async function save(event: FormEvent) {
    event.preventDefault();
    if (!current || operator || busy || !changed) return;
    setError(""); setSaved(false); setBusy(true);
    try {
      const result = await api<Settings>("proxy", mode === "custom" ? { mode, url: url.trim() } : { mode });
      setCurrent(result); setMode(result.mode); setUrl(""); setSaved(true);
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  }
  return <section className="operations proxy-panel"><div className="operation">
    <div><h2>{t("实例代理")}</h2><p>{t("此设置适用于所有用户的 GitHub 授权、收藏同步和公开 README 抓取。")}</p></div>
    {current?.address && <dl className="inline-fact"><dt>{t("当前代理")}</dt><dd>{current.address} {current.has_credentials && t("（已配置认证）")}</dd></dl>}
    {operator && <p>{t("此代理由部署环境中的 STARSTACK_PROXY_URL 管理。")}</p>}
    <form className="proxy-form" onSubmit={save}>
      <EuiFormRow label={t("连接方式")} fullWidth><EuiSelect fullWidth value={mode} disabled={!current || operator || busy}
        options={[{ value: "system", text: t("跟随系统代理") }, { value: "direct", text: t("直接连接") }, { value: "custom", text: t("指定代理") }]}
        onChange={e => { setMode(e.target.value as Mode); setUrl(""); setSaved(false); }} /></EuiFormRow>
      {mode === "custom" && !operator && <EuiFormRow label={t("新代理 URL")} fullWidth
        helpText={t("支持 HTTP、HTTPS、SOCKS5 和 SOCKS5h。替换地址时请同时填入所需的认证信息。")}>
        <EuiFieldPassword fullWidth type="dual" value={url} disabled={!current || busy} autoComplete="new-password"
          placeholder="socks5h://proxy.example:1080" onChange={e => { setUrl(e.target.value); setSaved(false); }} />
      </EuiFormRow>}
      {!operator && <EuiButton type="submit" isLoading={busy} isDisabled={!current || !changed}>{t("保存代理")}</EuiButton>}
    </form>
    {error && <Failure error={error} retry={!current ? () => setRevision(n => n + 1) : undefined} />}
    {saved && <p role="status">{t("代理设置已保存。")}</p>}
  </div></section>;
}

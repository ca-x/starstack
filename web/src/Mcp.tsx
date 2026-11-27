import { t } from "./locale";
import { useEffect, useState, type FormEvent } from "react";
import { EuiButton, EuiButtonEmpty, EuiFieldText, EuiFormRow, EuiSwitch } from "@elastic/eui";
import { api, date, message } from "./api";
import { Failure } from "./shared";

interface Token { id: string; name: string; created_at: string }

function ModelPreference() {
  const [enabled, setEnabled] = useState<boolean>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    api<{ use_chat: boolean }>("mcp/settings", undefined, controller.signal)
      .then(data => { if (!controller.signal.aborted) setEnabled(data.use_chat); })
      .catch(e => { if (!controller.signal.aborted) setError(message(e)); });
    return () => controller.abort();
  }, [revision]);
  async function update(useChat: boolean) {
    setBusy(true); setError("");
    try {
      const data = await api<{ use_chat: boolean }>("mcp/settings", { use_chat: useChat });
      setEnabled(data.use_chat);
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  }
  return <section className="operation">
    <div><h2>{t("检索与推荐")}</h2><p>{t("MCP 自动使用此账号的语义索引。开启后，聊天模型先为原问题补充检索词，再根据最多五个仓库的名称、描述及语言生成建议。")}</p></div>
    <EuiSwitch label={t("在 MCP 检索中使用 AI 聊天模型")} checked={enabled ?? false}
      disabled={enabled === undefined || busy} onChange={event => update(event.target.checked)} />
    {error && <Failure error={error} retry={enabled === undefined ? () => { setError(""); setRevision(n => n + 1); } : undefined} />}
  </section>;
}

export function Mcp() {
  const [tokens, setTokens] = useState<Token[]>([]);
  const [name, setName] = useState("");
  const [secret, setSecret] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [revision, setRevision] = useState(0);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState("");
  const [listError, setListError] = useState("");
  const [status, setStatus] = useState("");
  useEffect(() => {
    const controller = new AbortController();
    setListError("");
    api<{ tokens: Token[] }>("tokens", undefined, controller.signal).then(data => {
      if (!controller.signal.aborted) { setTokens(data.tokens); setLoaded(true); }
    }).catch(e => { if (!controller.signal.aborted) setListError(message(e)); });
    return () => controller.abort();
  }, [revision]);

  async function create(e: FormEvent) {
    e.preventDefault();
    if (busy || !name.trim()) return;
    setBusy(true); setError(""); setStatus("");
    try {
      const result = await api<{ token: string }>("tokens", { name: name.trim() });
      setSecret(result.token); setName(""); setRevision(n => n + 1);
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  }

  async function revoke(id: string) {
    setBusy(true); setError(""); setStatus("");
    try {
      await api("tokens/revoke", { id });
      setConfirm(""); setStatus(t("令牌已撤销。")); setRevision(n => n + 1);
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  }

  return <>
    <div className="page-heading"><div><h1>{t("MCP 接入")}</h1><p>{t("在支持 Streamable HTTP 的 MCP 客户端中访问收藏。")}</p></div></div>
    <div className="operations">
      <section className="operation"><h2>{t("连接地址")}</h2><code className="endpoint">{location.origin}/mcp</code>
        <p>{t("在客户端设置请求头")}<code>{t("Authorization: Bearer <令牌>")}</code></p></section>
      <ModelPreference />
      <section className="operation"><h2>{t("访问令牌")}</h2>
        {secret ? <div className="token-secret"><EuiFormRow label={t("新令牌")} helpText={t("完整令牌仅在这里显示一次，请复制并妥善保存。")} fullWidth>
          <EuiFieldText fullWidth value={secret} readOnly onFocus={e => e.target.select()} /></EuiFormRow>
          <EuiButton size="s" onClick={() => setSecret("")}>{t("已保存，隐藏令牌")}</EuiButton></div> :
          <form className="create-token" onSubmit={create}><EuiFormRow label={t("令牌名称")}><EuiFieldText required value={name} onChange={e => setName(e.target.value)} placeholder={t("例如「桌面客户端」")} /></EuiFormRow>
            <EuiButton fill type="submit" isDisabled={!name.trim()} isLoading={busy}>{t("生成令牌")}</EuiButton></form>}
        {error && <Failure error={error} />}{status && <p role="status">{status}</p>}
        {listError ? <Failure error={listError} retry={() => setRevision(n => n + 1)} /> : !loaded ? <p role="status">{t("正在读取令牌…")}</p> :
          tokens.length ? <ul className="token-list">{tokens.map(token => <li key={token.id}>
            <div><strong>{token.name}</strong><small>{t("创建于")} {date(token.created_at, true)}</small></div>
            <div className="row-controls">{confirm === token.id ? <><EuiButton size="s" color="danger" isLoading={busy} onClick={() => revoke(token.id)}>{t("确认撤销")}</EuiButton>
              <EuiButtonEmpty size="s" isDisabled={busy} onClick={() => setConfirm("")}>{t("取消")}</EuiButtonEmpty></> :
              <EuiButtonEmpty size="s" color="danger" isDisabled={busy} onClick={() => setConfirm(token.id)}>{t("撤销")}</EuiButtonEmpty>}</div></li>)}</ul> : <p>{t("暂无访问令牌。")}</p>}
      </section>
    </div>
  </>;
}

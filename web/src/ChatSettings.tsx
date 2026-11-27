import { useEffect, useState, type FormEvent } from "react";
import { EuiButton, EuiButtonEmpty, EuiCheckbox, EuiComboBox, EuiFieldPassword, EuiFieldText, EuiFormRow, EuiSelect } from "@elastic/eui";
import { api, message } from "./api";
import { t } from "./locale";
import { Failure } from "./shared";

type ApiType = "auto" | "responses" | "chat_completions";

interface Settings {
  configured: boolean; provider: string | null; model: string | null; base_url: string | null;
  api_type: ApiType; has_key: boolean; managed_by_env: boolean;
}
const apiTypes: { value: ApiType; text: string }[] = [
  { value: "auto", text: "自动（服务商默认）" },
  { value: "responses", text: "Responses · /responses" },
  { value: "chat_completions", text: "Chat Completions · /chat/completions" },
];
const providers = [
  { value: "ollama", text: "Ollama" }, { value: "openai", text: "OpenAI" },
  { value: "openai-compatible", text: "OpenAI-compatible" },
  { value: "anthropic", text: "Anthropic" }, { value: "openrouter", text: "OpenRouter" },
];
function selection(data: Settings) {
  if (data.provider === "openai" && data.base_url) return "openai-compatible";
  return data.provider || "";
}
function validAddress(text: string) {
  try {
    const url = new URL(text);
    return /^https?:$/.test(url.protocol) && !!url.hostname && !url.username && !url.password && !url.search && !url.hash;
  } catch { return false; }
}

export function ChatSettings({ admin, active }: { admin: boolean; active: boolean }) {
  const [current, setCurrent] = useState<Settings>();
  const [provider, setProvider] = useState("");
  const [apiType, setApiType] = useState<ApiType>("auto");
  const [model, setModel] = useState("");
  const [url, setUrl] = useState("");
  const [key, setKey] = useState("");
  const [clearKey, setClearKey] = useState(false);
  const [revision, setRevision] = useState(0);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [tested, setTested] = useState<number>();
  const [testError, setTestError] = useState("");
  const [saved, setSaved] = useState(false);
  const [models, setModels] = useState<string[]>([]);
  const [modelBusy, setModelBusy] = useState(false);
  const [modelError, setModelError] = useState("");
  const [modelRevision, setModelRevision] = useState(0);
  const [manual, setManual] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    const controller = new AbortController();
    api<Settings>("chat", undefined, controller.signal).then(data => {
      if (controller.signal.aborted) return;
      setCurrent(data); setProvider(selection(data)); setApiType(data.api_type ?? "auto"); setModel(data.model || ""); setUrl(data.base_url || "");
      setError("");
    }).catch(e => { if (!controller.signal.aborted) setError(message(e)); });
    return () => controller.abort();
  }, [revision]);

  const selectedProvider = provider === "openai-compatible" ? "openai" : provider;
  const address = provider ? url.trim() : "";
  const modelName = provider ? model.trim() : "";
  const changedService = selectedProvider !== (current?.provider || "") || address !== (current?.base_url || "");
  const needsUrl = provider === "ollama" || provider === "openai-compatible";
  const invalidAddress = address ? !validAddress(address) : needsUrl;
  const working = saving || testing;
  const changed = current && (apiType !== (current.api_type ?? "auto") || selection(current) !== provider || changedService || modelName !== (current.model || "") || !!key || clearKey);
  const editable = admin && current && !current.managed_by_env;
  const reuseKey = !!current?.has_key && !changedService && !clearKey;
  const requiresKey = provider === "openai" || provider === "anthropic";
  const discover = !!(editable && active && provider && !invalidAddress && (!requiresKey || key || reuseKey));
  useEffect(() => {
    setModels([]); setModelError(""); setModelBusy(discover);
    if (!discover) return;
    const controller = new AbortController();
    const timer = window.setTimeout(async () => {
      try {
        const response = await api<{ models: string[] }>("chat/models", { provider: selectedProvider, base_url: address,
          ...(key ? { api_key: key } : { clear_key: clearKey }) }, controller.signal);
        if (!controller.signal.aborted) setModels(response.models);
      } catch (e) {
        if (!controller.signal.aborted) setModelError(message(e));
      } finally {
        if (!controller.signal.aborted) setModelBusy(false);
      }
    }, 550);
    return () => { clearTimeout(timer); controller.abort(); };
  }, [discover, selectedProvider, address, key, clearKey, modelRevision]);
  const canSave = editable && !working && changed && (!provider || (modelName && !invalidAddress));
  const canTest = editable && !working && !!provider && !!modelName && !invalidAddress;
  const candidate = {
    provider: selectedProvider, model: modelName, base_url: address, api_type: apiType,
    ...(key ? { api_key: key } : (clearKey || (current?.has_key && changedService)) ? { clear_key: true } : {}),
  };
  function edited() {
    setSaved(false); setTested(undefined); setTestError("");
  }
  function select(value: string) {
    setProvider(value); setManual(false); edited(); setError(""); setKey(""); setClearKey(false);
    if (current && value === selection(current)) { setModel(current.model || ""); setUrl(current.base_url || ""); setApiType(current.api_type ?? "auto"); }
    else { setModel(""); setUrl(""); setApiType(value === "openai-compatible" ? "chat_completions" : "auto"); }
  }
  async function save(event: FormEvent) {
    event.preventDefault();
    if (!canSave) return;
    setSaving(true); setError(""); setSaved(false);
    try {
      await api("chat", candidate);
      setKey(""); setClearKey(false); setSaved(true); setRevision(n => n + 1);
    } catch (e) { setError(message(e)); } finally { setSaving(false); }
  }
  async function test() {
    if (!canTest) return;
    setTesting(true); setTestError(""); setTested(undefined);
    try { setTested((await api<{ elapsed_ms: number }>("chat/test", candidate)).elapsed_ms); }
    catch (e) { setTestError(message(e)); } finally { setTesting(false); }
  }
  return <section className="operation"><div className="chat-settings"><h2>{t("AI 聊天服务")}</h2>
    {current && <p role="status">{!current.configured ? t("尚未配置 AI 聊天服务。") :
      admin && current.provider ? t("当前聊天服务为 {0} · {1}。", current.provider, current.model || "") : t("AI 聊天服务已配置。")}</p>}
    {admin && current?.managed_by_env && <p>{t("此服务由部署环境配置。")}
      {current.provider === "openai" && <> {t("OpenAI 请求类型")} · {t(apiTypes.find(item => item.value === current.api_type)?.text || apiTypes[0].text)}</>}</p>}
    {editable && <form className="model-selector" onSubmit={save}>
      <EuiFormRow label={t("聊天服务商")} fullWidth><EuiSelect fullWidth disabled={working} value={provider} onChange={e => select(e.target.value)}
        options={[{ value: "", text: t("未配置") }, ...providers, ...(provider && !providers.some(p => p.value === provider) ? [{ value: provider, text: provider }] : [])]} /></EuiFormRow>
      {provider && <>
        <EuiFormRow label={t(needsUrl ? "聊天服务地址" : "聊天服务地址（可选）")} fullWidth
          helpText={provider === "ollama" ? t("例如 http://server:11434/v1") : selectedProvider === "openai" ? t("例如 https://gateway.example.com/v1。只填写 API 基址。") : undefined} isInvalid={!!address && invalidAddress}
          error={t("请输入不含凭证、查询参数或片段的 HTTP(S) 地址。")}><EuiFieldText fullWidth type="url" required={needsUrl}
            disabled={working} value={url} onChange={e => { setUrl(e.target.value); edited(); }} /></EuiFormRow>
        {selectedProvider === "openai" && <EuiFormRow label={t("OpenAI 请求类型")} fullWidth>
          <EuiSelect fullWidth disabled={working} value={apiType}
            options={apiTypes.map(item => ({ value: item.value, text: t(item.text) }))}
            onChange={e => { setApiType(e.target.value as ApiType); edited(); }} />
        </EuiFormRow>}
        <EuiFormRow label={t("新 API 密钥")} helpText={current.has_key ? t(changedService ? "更换服务商或地址会清除原密钥。需要认证时请填写新密钥。" : "已设置密钥。留空可保留现有密钥。") : undefined} fullWidth>
          <EuiFieldPassword fullWidth autoComplete="new-password" disabled={working || clearKey} value={key} onChange={e => { setKey(e.target.value); edited(); }} /></EuiFormRow>
        {current.has_key && !changedService && <EuiCheckbox id="chat-clear-key" label={t("清除已保存的密钥")} checked={clearKey} disabled={working}
          onChange={e => { setClearKey(e.target.checked); setKey(""); edited(); }} />}
        <EuiFormRow label={t("对话模型名称")} fullWidth helpText={
          !discover ? t(requiresKey && !key && !reuseKey ? "填写 API 密钥后自动获取模型。" : "填写服务地址后自动获取模型。") :
          modelBusy ? t("正在获取模型列表…") : !modelError && !models.length ? t("服务未返回可选模型。") : undefined}>
          {manual
            ? <EuiFieldText fullWidth required disabled={working} value={model} onChange={e => { setModel(e.target.value); edited(); }} />
            : <EuiComboBox fullWidth singleSelection={{ asPlainText: true }} placeholder={t("搜索并选择模型")}
              isLoading={modelBusy} isDisabled={working || !models.length}
              options={models.map(id => ({ label: id }))} selectedOptions={model ? [{ label: model }] : []}
              onChange={items => { setModel(items[0]?.label ?? ""); edited(); }} />}
        </EuiFormRow>
        {modelError && <Failure error={modelError} retry={() => setModelRevision(n => n + 1)} />}
        {(manual || discover && !modelBusy && !models.length) && <EuiButtonEmpty size="s" onClick={() => setManual(!manual)}>
          {t(manual ? "从列表选择模型" : "手动填写模型")}</EuiButtonEmpty>}
      </>}
      <div className="row-controls">
        {provider && <EuiButton type="button" isDisabled={!canTest} isLoading={testing} onClick={test}>{t("测试模型")}</EuiButton>}
        <EuiButton type="submit" fill isDisabled={!canSave} isLoading={saving}>{t("保存聊天服务")}</EuiButton>
      </div>
      {testError && <Failure error={testError} />}
      {tested !== undefined && <p role="status">{t("模型响应正常，耗时 {0} 毫秒。", tested)}</p>}
    </form>}
    {error && <Failure error={error} retry={!current ? () => setRevision(n => n + 1) : undefined} />}
    {saved && <p role="status">{t("聊天服务设置已保存。")}</p>}
  </div></section>;
}

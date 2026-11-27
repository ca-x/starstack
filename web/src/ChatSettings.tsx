import { useEffect, useState, type FormEvent } from "react";
import { EuiButton, EuiButtonEmpty, EuiCheckbox, EuiComboBox, EuiFieldPassword, EuiFieldText, EuiFormRow, EuiSelect } from "@elastic/eui";
import { api, message } from "./api";
import { t } from "./locale";
import { Failure } from "./shared";

interface Settings {
  configured: boolean; provider: string | null; model: string | null; base_url: string | null;
  has_key: boolean; managed_by_env: boolean;
}
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
  const [model, setModel] = useState("");
  const [url, setUrl] = useState("");
  const [key, setKey] = useState("");
  const [clearKey, setClearKey] = useState(false);
  const [revision, setRevision] = useState(0);
  const [saving, setSaving] = useState(false);
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
      setCurrent(data); setProvider(selection(data)); setModel(data.model || ""); setUrl(data.base_url || "");
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
  const changed = current && (selection(current) !== provider || changedService || modelName !== (current.model || "") || !!key || clearKey);
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
  const canSave = editable && !saving && changed && (!provider || (modelName && !invalidAddress));
  function select(value: string) {
    setProvider(value); setManual(false); setSaved(false); setError(""); setKey(""); setClearKey(false);
    if (current && value === selection(current)) { setModel(current.model || ""); setUrl(current.base_url || ""); }
    else { setModel(""); setUrl(""); }
  }
  async function save(event: FormEvent) {
    event.preventDefault();
    if (!canSave) return;
    setSaving(true); setError(""); setSaved(false);
    try {
      await api("chat", { provider: selectedProvider, model: modelName, base_url: address,
        ...(key ? { api_key: key } : (clearKey || (current.has_key && changedService)) ? { clear_key: true } : {}) });
      setKey(""); setClearKey(false); setSaved(true); setRevision(n => n + 1);
    } catch (e) { setError(message(e)); } finally { setSaving(false); }
  }
  return <section className="operation"><div className="chat-settings"><h2>{t("AI 聊天服务")}</h2>
    {current && <p role="status">{!current.configured ? t("尚未配置 AI 聊天服务。") :
      admin && current.provider ? t("当前聊天服务为 {0} · {1}。", current.provider, current.model || "") : t("AI 聊天服务已配置。")}</p>}
    {admin && current?.managed_by_env && <p>{t("此服务由部署环境配置。")}</p>}
    {editable && <form className="model-selector" onSubmit={save}>
      <EuiFormRow label={t("聊天服务商")} fullWidth><EuiSelect fullWidth disabled={saving} value={provider} onChange={e => select(e.target.value)}
        options={[{ value: "", text: t("未配置") }, ...providers, ...(provider && !providers.some(p => p.value === provider) ? [{ value: provider, text: provider }] : [])]} /></EuiFormRow>
      {provider && <>
        <EuiFormRow label={t(needsUrl ? "聊天服务地址" : "聊天服务地址（可选）")} fullWidth
          helpText={provider === "ollama" ? t("例如 http://server:11434/v1") : undefined} isInvalid={!!address && invalidAddress}
          error={t("请输入不含凭证、查询参数或片段的 HTTP(S) 地址。")}><EuiFieldText fullWidth type="url" required={needsUrl}
            disabled={saving} value={url} onChange={e => { setUrl(e.target.value); setSaved(false); }} /></EuiFormRow>
        <EuiFormRow label={t("新 API 密钥")} helpText={current.has_key ? t(changedService ? "更换服务商或地址会清除原密钥。需要认证时请填写新密钥。" : "已设置密钥。留空可保留现有密钥。") : undefined} fullWidth>
          <EuiFieldPassword fullWidth autoComplete="new-password" disabled={saving || clearKey} value={key} onChange={e => { setKey(e.target.value); setSaved(false); }} /></EuiFormRow>
        {current.has_key && !changedService && <EuiCheckbox id="chat-clear-key" label={t("清除已保存的密钥")} checked={clearKey} disabled={saving}
          onChange={e => { setClearKey(e.target.checked); setKey(""); setSaved(false); }} />}
        <EuiFormRow label={t("对话模型名称")} fullWidth helpText={
          !discover ? t(requiresKey && !key && !reuseKey ? "填写 API 密钥后自动获取模型。" : "填写服务地址后自动获取模型。") :
          modelBusy ? t("正在获取模型列表…") : !modelError && !models.length ? t("服务未返回可选模型。") : undefined}>
          {manual
            ? <EuiFieldText fullWidth required disabled={saving} value={model} onChange={e => { setModel(e.target.value); setSaved(false); }} />
            : <EuiComboBox fullWidth singleSelection={{ asPlainText: true }} placeholder={t("搜索并选择模型")}
              isLoading={modelBusy} isDisabled={saving || !models.length}
              options={models.map(id => ({ label: id }))} selectedOptions={model ? [{ label: model }] : []}
              onChange={items => { setModel(items[0]?.label ?? ""); setSaved(false); }} />}
        </EuiFormRow>
        {modelError && <Failure error={modelError} retry={() => setModelRevision(n => n + 1)} />}
        {(manual || discover && !modelBusy && !models.length) && <EuiButtonEmpty size="s" onClick={() => setManual(!manual)}>
          {t(manual ? "从列表选择模型" : "手动填写模型")}</EuiButtonEmpty>}
      </>}
      <EuiButton type="submit" isDisabled={!canSave} isLoading={saving}>{t("保存聊天服务")}</EuiButton>
    </form>}
    {error && <Failure error={error} retry={!current ? () => setRevision(n => n + 1) : undefined} />}
    {saved && <p role="status">{t("聊天服务设置已保存。")}</p>}
  </div></section>;
}

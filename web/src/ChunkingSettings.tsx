import { useState, type FormEvent } from "react";
import { EuiButton, EuiFormRow, EuiSelect } from "@elastic/eui";
import { t } from "./locale";

export interface Chunking {
  preset: "balanced" | "short" | "long" | "whole";
}

export function ChunkingSettings({ current, disabled, save }: {
  current: Chunking; disabled: boolean; save: (settings: Chunking) => Promise<boolean>;
}) {
  const [draft, setDraft] = useState<Chunking["preset"]>();
  const preset = draft ?? current.preset;
  const canSave = !disabled && preset !== current.preset;

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (canSave && await save({ preset })) setDraft(undefined);
  }

  return <form className="model-selector" onSubmit={submit}>
    <EuiFormRow label={t("分块预设")} helpText={t("调整当前账号发送给 Ollama 的每个文本块的长度。")} fullWidth>
      <EuiSelect fullWidth value={preset} disabled={disabled} onChange={e => setDraft(e.target.value as Chunking["preset"])}
        options={[{ value: "balanced", text: t("均衡（推荐）") }, { value: "short", text: t("短分块") },
          { value: "long", text: t("长分块") }, { value: "whole", text: t("整篇（兼容）") }]} />
    </EuiFormRow>
    <EuiButton type="submit" isDisabled={!canSave}>{t("保存分块设置")}</EuiButton>
  </form>;
}

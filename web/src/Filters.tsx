import { languageName, t } from "./locale";
import { useState, type FormEvent } from "react";
import { EuiButton, EuiFieldText, EuiFormRow, EuiSelect } from "@elastic/eui";
import { counts, type Filters as FilterValues, type Stats } from "./api";

export function FilterPanel({ initial, stats, fixedTag, apply }: { initial: FilterValues; stats: Stats; fixedTag?: string; apply: (filters: FilterValues) => void }) {
  const [draft, setDraft] = useState(initial);
  const lists = counts(stats.lists ?? {});
  const listOptions = [
    { value: "", text: t("全部") },
    ...lists.map(([name, count]) => ({ value: name, text: t("{0}（{1}）", name, count.toLocaleString()) })),
  ];
  if (draft.list && !lists.some(([name]) => name === draft.list)) {
    listOptions.push({ value: draft.list, text: draft.list });
  }
  const update = (key: keyof FilterValues, value: string | boolean | undefined) => setDraft(s => ({ ...s, [key]: value === "" ? undefined : value }));
  function submit(e: FormEvent) {
    e.preventDefault();
    apply({ ...draft, topic: draft.topic?.trim() || undefined });
  }
  return <form className="filters" onSubmit={submit}>
    {(["language", "tag"] as const).map(key => <EuiFormRow key={key} label={key === "language" ? t("语言") : t("我的标签")} fullWidth>
      <EuiSelect fullWidth disabled={key === "tag" && fixedTag !== undefined} value={draft[key] || ""} onChange={e => update(key, e.target.value)}
        options={[{ value: "", text: t("全部") }, ...counts(key === "language" ? stats.languages : stats.tags).map(([value]) => ({ value, text: key === "language" ? languageName(value) : value }))]} />
    </EuiFormRow>)}
    <EuiFormRow label={t("GitHub 收藏分类")} fullWidth
      helpText={!lists.length ? t("暂无可用分类。请确认 GitHub 授权后同步收藏。") : undefined}>
      <EuiSelect fullWidth disabled={!lists.length && !draft.list} value={draft.list || ""}
        options={listOptions} onChange={e => update("list", e.target.value)} />
    </EuiFormRow>
    <EuiFormRow label={t("GitHub 主题")} fullWidth><EuiFieldText fullWidth value={draft.topic || ""} onChange={e => update("topic", e.target.value)} /></EuiFormRow>
    <EuiFormRow label={t("仓库状态")} fullWidth><EuiSelect fullWidth value={String(draft.archived ?? "")} onChange={e => update("archived", e.target.value === "" ? undefined : e.target.value === "true")}
      options={[{ value: "", text: t("全部") }, { value: "false", text: t("活跃") }, { value: "true", text: t("已归档") }]} /></EuiFormRow>
    <label className="date-label">{t("收藏起始日期")}<input type="date" value={draft.since || ""} max={draft.until} onChange={e => update("since", e.target.value)} /></label>
    <label className="date-label">{t("收藏截止日期")}<input type="date" value={draft.until || ""} min={draft.since} onChange={e => update("until", e.target.value)} /></label>
    <div className="filter-actions"><EuiButton type="submit" size="s" fill>{t("应用筛选")}</EuiButton>
      <EuiButton type="button" size="s" onClick={() => apply({ pinned: initial.pinned, tag: fixedTag })}>{t("清除筛选")}</EuiButton></div>
  </form>;
}

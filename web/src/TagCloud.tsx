import { useState } from "react";
import { EuiButtonEmpty } from "@elastic/eui";
import { counts } from "./api";
import { t } from "./locale";

export function TagCloud({ values, tagLink, compact = false }: {
  values: Record<string, number>; tagLink: (tag: string) => string; compact?: boolean;
}) {
  const tags = counts(values);
  const [expanded, setExpanded] = useState(false);
  const visible = compact && !expanded ? tags.slice(0, 12) : tags;
  const largest = Math.log1p(tags[0]?.[1] ?? 0);
  const smallest = Math.log1p(tags.at(-1)?.[1] ?? 0);
  return <section className={`stat-panel tag-cloud-panel${compact ? " home-tags" : ""}`} aria-labelledby="tag-cloud-heading">
    <div className="row-controls"><h2 id="tag-cloud-heading">{t(compact ? "我的标签" : "本地标签云")}</h2>
      {compact && tags.length > 12 && <EuiButtonEmpty size="s" aria-expanded={expanded} onClick={() => setExpanded(value => !value)}>
        {t(expanded ? "收起标签" : "查看全部标签")}</EuiButtonEmpty>}</div>
      {tags.length ? <ul className="tag-cloud">{visible.map(([name, n]) => {
        const level = largest === smallest ? 1 : Math.round(3 * (Math.log1p(n) - smallest) / (largest - smallest));
        return <li key={name}><a href={tagLink(name)} className={`tag-level-${level}`} aria-label={t("标签 {0}，{1} 个仓库", name, n.toLocaleString())}>
          <span>{name}</span><small>{n.toLocaleString()}</small>
        </a></li>;
      })}</ul> : <p className="subdued">{t("在仓库详情中添加标签后，可在这里查看使用次数。")}</p>}
  </section>;
}

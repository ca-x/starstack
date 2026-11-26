import { t } from "./locale";
import { counts, type Stats } from "./api";
import { Language } from "./shared";

export function Statistics({ stats, tagLink }: { stats: Stats; tagLink: (tag: string) => string }) {
  const tags = counts(stats.tags);
  const languages = counts(stats.languages);
  const largest = Math.log1p(tags[0]?.[1] ?? 0);
  const smallest = Math.log1p(tags.at(-1)?.[1] ?? 0);
  return <>
    <div className="page-heading"><div><h1>{t("收藏统计")}</h1><p>{t("查看收藏规模、语言分布和标签使用情况。")}</p></div></div>
    <dl className="stat-strip">{[[t("收藏仓库"), stats.total], [t("编程语言"), Object.keys(stats.languages).filter(name => name !== "未标注").length], [t("我的标签"), Object.keys(stats.tags).length]].map(([key, n]) =>
      <div key={key}><dt>{key}</dt><dd>{n.toLocaleString()}</dd></div>)}</dl>
    <div className="stat-grid"><section className="stat-panel"><h2>{t("编程语言")}</h2>
      {languages.length ? <ul>{languages.map(([name, n]) => <li key={name}>
        <div><Language name={name} /><b>{n.toLocaleString()}</b></div>
        <span className="bar" aria-hidden="true"><i style={{ width: `${n / Math.max(1, stats.total) * 100}%` }} /></span>
      </li>)}</ul> : <p className="subdued">{t("同步收藏后可查看语言分布。")}</p>}
    </section><section className="stat-panel tag-cloud-panel" aria-labelledby="tag-cloud-heading"><h2 id="tag-cloud-heading">{t("本地标签云")}</h2>
      {tags.length ? <ul className="tag-cloud">{tags.map(([name, n]) => {
        const level = largest === smallest ? 1 : Math.round(3 * (Math.log1p(n) - smallest) / (largest - smallest));
        return <li key={name}><a href={tagLink(name)} className={`tag-level-${level}`} aria-label={t("标签 {0}，{1} 个仓库", name, n.toLocaleString())}>
          <span>{name}</span><small>{n.toLocaleString()}</small>
        </a></li>;
      })}</ul> : <p className="subdued">{t("在仓库详情中添加标签后，可在这里查看使用次数。")}</p>}
    </section></div>
  </>;
}

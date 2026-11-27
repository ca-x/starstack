import { t } from "./locale";
import { counts, type Stats } from "./api";
import { TagCloud } from "./TagCloud";
import { Language } from "./shared";

export function Statistics({ stats, tagLink }: { stats: Stats; tagLink: (tag: string) => string }) {
  const languages = counts(stats.languages);
  return <>
    <div className="page-heading"><div><h1>{t("收藏统计")}</h1><p>{t("查看收藏规模、语言分布和标签使用情况。")}</p></div></div>
    <dl className="stat-strip">{[[t("收藏仓库"), stats.total], [t("编程语言"), Object.keys(stats.languages).filter(name => name !== "未标注").length], [t("我的标签"), Object.keys(stats.tags).length]].map(([key, n]) =>
      <div key={key}><dt>{key}</dt><dd>{n.toLocaleString()}</dd></div>)}</dl>
    <div className="stat-grid"><section className="stat-panel"><h2>{t("编程语言")}</h2>
      {languages.length ? <ul>{languages.map(([name, n]) => <li key={name}>
        <div><Language name={name} /><b>{n.toLocaleString()}</b></div>
        <span className="bar" aria-hidden="true"><i style={{ width: `${n / Math.max(1, stats.total) * 100}%` }} /></span>
      </li>)}</ul> : <p className="subdued">{t("同步收藏后可查看语言分布。")}</p>}
    </section><TagCloud values={stats.tags} tagLink={tagLink} /></div>
  </>;
}

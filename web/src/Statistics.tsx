import { t } from "./locale";
import { counts, type Stats } from "./api";
import { Language } from "./shared";

export function Statistics({ stats, tagLink }: { stats: Stats; tagLink: (tag: string) => string }) {
  return <>
    <div className="page-heading"><div><h1>{t("收藏统计")}</h1><p>{t("查看收藏规模、语言分布和标签使用情况。")}</p></div></div>
    <dl className="stat-strip">{[[t("收藏仓库"), stats.total], [t("编程语言"), Object.keys(stats.languages).filter(name => name !== "未标注").length], [t("我的标签"), Object.keys(stats.tags).length]].map(([key, n]) =>
      <div key={key}><dt>{key}</dt><dd>{n.toLocaleString()}</dd></div>)}</dl>
    <div className="stat-grid">{(["languages", "tags"] as const).map(key => <section className="stat-panel" key={key}>
      <h2>{key === "languages" ? t("编程语言") : t("我的标签")}</h2>
      {counts(stats[key]).length ? <ul>{counts(stats[key]).map(([name, n]) => <li key={name}>
        <div>{key === "languages" ? <Language name={name} /> : <a href={tagLink(name)}>{name}</a>}<b>{n.toLocaleString()}</b></div>
        <span className="bar" aria-hidden="true"><i style={{ width: `${n / Math.max(1, stats.total) * 100}%` }} /></span>
      </li>)}</ul> : <p className="subdued">{key === "tags" ? t("在仓库详情中添加标签后，可在这里查看使用次数。") : t("同步收藏后可查看语言分布。")}</p>}
    </section>)}</div>
  </>;
}

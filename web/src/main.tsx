import { t } from "./locale";
import { useCallback, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { EuiButtonEmpty } from "@elastic/eui";
import { Preferences, PreferenceMenu } from "./Preferences";
import { api, message, request, type Stats } from "./api";
import { Library } from "./Library";
import { Login } from "./Login";
import { Mcp } from "./Mcp";
import { Maintenance, type Job, type JobReport, type IndexProgress } from "./Maintenance";
import { Statistics } from "./Statistics";
import { Users } from "./Users";
import { ProfileButton, type Profile } from "./Profile";
import { Brand, Failure, Icon } from "./shared";

import libraryIcon from "./icons/stack-library.svg?no-inline";
import pinnedIcon from "./icons/stack-pinned.svg?no-inline";
import statsIcon from "./icons/stack-stats.svg?no-inline";
import syncIcon from "./icons/stack-sync.svg?no-inline";
import usersIcon from "./icons/stack-users.svg?no-inline";
import mcpIcon from "./icons/stack-key.svg?no-inline";
import "./styles.css";

const tagLink = (tag: string) => "#tag/" + encodeURIComponent(tag);
const navigation = () => [
  ["#library", t("全部收藏"), libraryIcon], ["#pinned", t("置顶仓库"), pinnedIcon],
  ["#statistics", t("收藏统计"), statsIcon], ["#sync", t("同步与索引"), syncIcon], ["#mcp", t("MCP 接入"), mcpIcon],
];
const current = () => location.hash || "#library";
function Admin() {
  const [route, setRoute] = useState(current);
  const [mobileNav, setMobileNav] = useState(false);
  const [stats, setStats] = useState<Stats>();
  const [profile, setProfile] = useState<Profile>();
  const authenticated = profile !== undefined;
  const [statsError, setStatsError] = useState("");
  const [revision, setRevision] = useState(0);
  const [job, setJob] = useState<Job | null>(null);
  const [progress, setProgress] = useState("");
  const [indexProgress, setIndexProgress] = useState<IndexProgress>();
  const [catalogReport, setCatalogReport] = useState<JobReport>();
  const [jobReport, setJobReport] = useState<JobReport>();
  const [jobError, setJobError] = useState("");
  const [externalRunning, setExternalRunning] = useState(false);
  const [loggingOut, setLoggingOut] = useState(false);
  const [logoutError, setLogoutError] = useState("");
  const busy = useRef(false);
  const menuButton = useRef<HTMLButtonElement>(null);
  const main = useRef<HTMLElement>(null);
  const refresh = useCallback(() => setRevision(n => n + 1), []);

  useEffect(() => {
    if (!mobileNav) return;
    const close = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.defaultPrevented) { setMobileNav(false); menuButton.current?.focus(); }
    };
    document.addEventListener("keydown", close);
    return () => document.removeEventListener("keydown", close);
  }, [mobileNav]);
  useEffect(() => {
    const change = () => { setRoute(current()); setMobileNav(false); main.current?.focus({ preventScroll: true }); window.scrollTo(0, 0); };
    window.addEventListener("hashchange", change);
    return () => window.removeEventListener("hashchange", change);
  }, []);
  useEffect(() => {
    const controller = new AbortController();
    setStatsError("");
    Promise.all([api<Stats>("stats", undefined, controller.signal), api<Profile>("me", undefined, controller.signal)])
      .then(([data, user]) => { if (!controller.signal.aborted) { setStats(data); setProfile(user); } })
      .catch(e => { if (!controller.signal.aborted) setStatsError(message(e)); });
    return () => controller.abort();
  }, [revision]);
  useEffect(() => {
    if (!job && (route !== "#sync" || !authenticated)) return;
    let timer: number;
    let previouslyRunning = false;
    const controller = new AbortController();
    const poll = async () => {
      let interval = 4000;
      try {
        const result = await api<{ message: string | null; running?: boolean; report?: JobReport | null; catalog_report?: JobReport | null; index_progress?: IndexProgress | null }>("progress", undefined, controller.signal);
        if (controller.signal.aborted) return;
        setIndexProgress(result.index_progress ?? undefined);
        setCatalogReport(result.catalog_report ?? undefined);
        if (busy.current) {
          if (result.message) setProgress(t(result.message));
          setJobReport(result.report ?? undefined);
          interval = 900;
        } else if (typeof result.running === "boolean") {
          setExternalRunning(result.running);
          setJobReport(result.report ?? undefined);
          if (result.running) { setJobError(""); interval = 900; }
          if (result.message && (result.running || result.report || !jobError)) setProgress(t(result.message));
          if (previouslyRunning && !result.running) refresh();
          previouslyRunning = result.running;
        }
      } catch { /* The next poll resumes when the connection is available. */ }
      if (!controller.signal.aborted) timer = window.setTimeout(poll, interval);
    };
    timer = window.setTimeout(poll, job ? 350 : 0);
    return () => { controller.abort(); clearTimeout(timer); };
  }, [job, route, authenticated, jobError, refresh]);

  async function run(next: Job, full: boolean, requestId?: string) {
    if (busy.current) return;
    busy.current = true;
    setJob(next); setJobError(""); setJobReport(undefined); setIndexProgress(undefined);
    setProgress(t({ sync: "正在同步收藏…", index: "正在更新索引…", readmes: "正在提交 README 补全任务…" }[next]));
    const input = { sync: requestId ? { request_id: requestId } : { full }, index: { rebuild: full }, readmes: {} }[next];
    try {
      const result = await api<JobReport>(next, input);
      if (next === "readmes") setExternalRunning(true);
      else setJobReport(result);
      setProgress(t(result.message || (next === "readmes" ? "已提交 README 补全任务。" : "操作已完成。")));
    } catch (e) { setJobError(message(e)); setProgress(""); }
    finally { busy.current = false; setJob(null); refresh(); }
  }
  async function logout() {
    setLoggingOut(true); setLogoutError("");
    try { await request("logout", {}); location.replace("/login"); }
    catch (e) { setLogoutError(message(e)); setLoggingOut(false); }
  }

  if (!stats || !profile) return <main className="checking">{statsError ? <Failure error={statsError} retry={refresh} /> : <p role="status">{t("正在打开 StarStack…")}</p>}</main>;
  let tag: string | undefined;
  if (route.startsWith("#tag/")) { try { tag = decodeURIComponent(route.slice(5)); } catch { /* Invalid link falls back to the library. */ } }
  const navItem = (href: string, name: string, icon: string, count?: number) => <a key={href} href={href}
    onClick={() => setMobileNav(false)} aria-current={route === href ? "page" : undefined}>
    <img src={icon} alt="" width="24" height="24" /><span>{name}</span>{count !== undefined && <small>{count.toLocaleString()}</small>}</a>;
  return <div className="shell">
    <a className="skip" href="#main" onClick={e => { e.preventDefault(); main.current?.focus(); }}>{t("跳到主要内容")}</a>
    <header className="topbar"><div className="header-brand"><button ref={menuButton} className="mobile-toggle" aria-label={t("切换导航")} aria-expanded={mobileNav}
      aria-controls="navigation" onClick={() => setMobileNav(!mobileNav)}><Icon type={mobileNav ? "cross" : "menu"} /></button><Brand /><span className="brand-divider">{t("收藏管理台")}</span></div>
      <div className="profile-menu"><PreferenceMenu /><ProfileButton profile={profile} changed={setProfile} /><EuiButtonEmpty size="s" aria-label={t("退出登录")} title={t("退出登录")} isDisabled={loggingOut} iconType="logOut" onClick={logout}><span className="signout-label">{t("退出登录")}</span></EuiButtonEmpty></div></header>
    <aside id="navigation" className={mobileNav ? "rail is-open" : "rail"}><nav aria-label={t("主导航")}>
      <span className="nav-label">{t("工作区")}</span><div className="nav-group">{navigation().map(([href, name, icon]) => navItem(href, name, icon, href === "#library" ? stats.total : undefined))}{profile.admin && navItem("#users", t("用户管理"), usersIcon)}</div>
    </nav></aside>
    <main id="main" className={`workspace${["#sync", "#users", "#mcp"].includes(route) ? " workspace--settings" : ""}`} tabIndex={-1} ref={main}>
      {statsError && <Failure error={statsError} retry={refresh} />}{logoutError && <Failure error={logoutError} />}
      {job && route !== "#sync" && <a href="#sync" className="progress-note">{progress || t("查看任务进度")} <Icon type="arrowRight" /></a>}
      {route === "#sync" ? <Maintenance stats={stats} admin={profile.admin} job={job || (externalRunning ? "sync" : null)} progress={progress} indexProgress={indexProgress} catalogReport={catalogReport} report={jobReport} error={jobError} run={run} /> :
        route === "#users" && profile.admin ? <Users username={profile.username} /> : route === "#mcp" ? <Mcp /> : route === "#statistics" ? <Statistics stats={stats} tagLink={tagLink} /> :
        <Library key={route} stats={stats} tagLink={tagLink} initial={{ pinned: route === "#pinned" || undefined, tag }} revision={revision} onEdit={refresh} onSync={() => { location.hash = "sync"; }} />}
    </main>
  </div>;
}

const isLogin = location.pathname.replace(/\/$/, "") === "/login";
if (!isLogin && location.pathname !== "/admin") history.replaceState(null, "", "/admin" + location.hash);
createRoot(document.getElementById("root")!).render(<Preferences app={isLogin ? Login : Admin} login={isLogin} />);

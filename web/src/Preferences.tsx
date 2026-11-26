import { createContext, useContext, useEffect, useLayoutEffect, useState, type ComponentType } from "react";
import { EuiButtonIcon, EuiContext, EuiFormRow, EuiPopover, EuiProvider, EuiSelect } from "@elastic/eui";
import { EuiThemeBorealis } from "@elastic/eui-theme-borealis";
import { getAppearance, getLocale, setLocale, t, write, type Locale, type Appearance } from "./locale";
import { consoleTheme } from "./theme";

const Context = createContext<{
  locale: Locale; appearance: Appearance; language: (v: Locale) => void; theme: (v: Appearance) => void;
}>({ locale: getLocale(), appearance: getAppearance(), language: () => {}, theme: () => {} });

const system = matchMedia("(prefers-color-scheme: dark)");
export function Preferences({ app: App, login }: { app: ComponentType; login: boolean }) {
  const [locale, updateLocale] = useState(getLocale);
  const [appearance, updateTheme] = useState(getAppearance);
  const [systemDark, setSystemDark] = useState(system.matches);
  const color = appearance === "system" ? (systemDark ? "dark" : "light") : appearance;
  function language(value: Locale) { setLocale(value); updateLocale(value); }
  function theme(value: Appearance) { write("starstack.theme", value); updateTheme(value); }
  useEffect(() => {
    const update = () => setSystemDark(system.matches);
    system.addEventListener("change", update);
    return () => system.removeEventListener("change", update);
  }, []);
  useLayoutEffect(() => {
    document.documentElement.lang = locale;
    document.documentElement.dataset.theme = color;
    document.title = t(login ? "登录 · StarStack" : "StarStack · 仓库收藏");
    document.querySelector('meta[name="theme-color"]')?.setAttribute("content", color === "dark" ? "#191613" : "#f5f1eb");
  }, [locale, color, login]);
  return <Context.Provider value={{ locale, appearance, language, theme }}>
    <EuiProvider theme={EuiThemeBorealis} colorMode={color} modify={consoleTheme}><EuiContext i18n={{
      locale, mapping: locale === "zh-CN" ? {
        "euiFieldPassword.showPassword": "显示密码", "euiFieldPassword.maskPassword": "隐藏密码",
        "euiFlyoutCloseButton.ariaLabel": "关闭", "euiModal.screenReaderModalDialog": "按 Esc 关闭。",
        "euiPopover.screenReaderAnnouncement": "按 Esc 关闭。",
      } : {},
    }}><App /></EuiContext></EuiProvider>
  </Context.Provider>;
}

export function PreferenceMenu() {
  const { locale, appearance, language, theme } = useContext(Context);
  const [open, setOpen] = useState(false);
  return <EuiPopover anchorPosition="downRight" isOpen={open} closePopover={() => setOpen(false)}
    button={<EuiButtonIcon aria-label={t("界面偏好")} title={t("界面偏好")} iconType="gear" onClick={() => setOpen(!open)} />}>
    <div className="display-options">
      <EuiFormRow label={t("显示语言")}><EuiSelect value={locale} options={[{ value: "zh-CN", text: "简体中文" }, { value: "en", text: "English" }]}
        onChange={e => language(e.target.value as Locale)} /></EuiFormRow>
      <EuiFormRow label={t("外观")}><EuiSelect value={appearance} options={[{ value: "system", text: t("跟随系统") }, { value: "light", text: t("浅色") }, { value: "dark", text: t("深色") }]}
        onChange={e => theme(e.target.value as Appearance)} /></EuiFormRow>
    </div>
  </EuiPopover>;
}

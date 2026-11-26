import { languageName, t } from "./locale";
import logo from "./assets/logo-display.webp";
import { EuiButton, EuiIcon, type IconType } from "@elastic/eui";

export function Brand() {
  return <span className="brand"><img className="brand-image" src={logo} alt="" width="42" height="42" />StarStack</span>;
}

export function Icon({ type }: { type: IconType }) {
  return <EuiIcon type={type} size="m" aria-hidden="true" />;
}

export function Failure({ error, retry }: { error: string; retry?: () => void }) {
  return <div className="failure" role="alert"><Icon type="alert" /><span>{error}</span>
    {retry && <EuiButton size="s" onClick={retry}>{t("重试")}</EuiButton>}
  </div>;
}

export function Language({ name }: { name: string }) {
  const colors: Record<string, string> = {
    Rust: "#a65835", Go: "#07869c", TypeScript: "#3178c6", JavaScript: "#b69510",
    Python: "#376e9b", Swift: "#d35a36", Java: "#aa6720", HTML: "#ca593b", CSS: "#7552a1",
    Shell: "#47864e", Ruby: "#aa3939", "C++": "#aa527f", C: "#626d7b", Kotlin: "#7961ae",
  };
  return <span className="language"><i style={{ background: colors[name] || "#70857d" }} />{languageName(name)}</span>;
}

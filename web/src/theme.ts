import type { EuiThemeModifications } from "@elastic/eui";

export const consoleTheme: EuiThemeModifications = {
  font: {
    family: "var(--serif)",
    familySerif: "var(--serif)",
    familyCode: "var(--mono)",
  },
  animation: {
    fast: "150ms",
    normal: "220ms",
    bounce: "cubic-bezier(0.23, 1, 0.32, 1)",
  },
  colors: {
    LIGHT: {
      primary: "#8b4c28",
      backgroundLightPrimary: "#f5e9dd",
      backgroundBaseHighlighted: "#faf7f2",
      backgroundBaseInteractiveOverlay: "rgba(49, 35, 25, .42)",
      primaryText: "#8b4c28",
      textPrimary: "#8b4c28",
      link: "#8b4c28",
      textSubdued: "#655a50",
      textParagraph: "#3c3028",
      textHeading: "#25201c",
      backgroundBasePlain: "#fffdf9",
      backgroundBaseSubdued: "#f4efe7",
      backgroundBaseInteractiveHover: "#f5eee6",
      borderBasePlain: "#ded5cd",
      backgroundFilledPrimary: "#8b4c28",
      backgroundBaseInteractiveSelect: "#f5e9dd",
    },
    DARK: {
      primary: "#e7ae80",
      backgroundLightPrimary: "#382b24",
      backgroundBaseHighlighted: "#282320",
      backgroundBaseInteractiveOverlay: "rgba(12, 10, 8, .7)",
      primaryText: "#e7ae80",
      textPrimary: "#e7ae80",
      link: "#e7ae80",
      textSubdued: "#c2b4a7",
      textParagraph: "#e3dbd3",
      textHeading: "#f5efe7",
      backgroundBasePlain: "#24201d",
      backgroundBaseSubdued: "#1c1916",
      backgroundBaseInteractiveHover: "#302823",
      borderBasePlain: "#494039",
      backgroundFilledPrimary: "#e7ae80",
      backgroundBaseInteractiveSelect: "#322720",
    },
  },
  components: {
    forms: {
      LIGHT: { background: "#fffdf9", border: "#a49a92", borderHovered: "#82746b", borderFocused: "#8b4c28" },
      DARK: { background: "#282522", border: "#84746a", borderHovered: "#aa998a", borderFocused: "#e7ae80" },
    },
  },
};

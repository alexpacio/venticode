import { t } from "../i18n/index.ts";

export function relative(iso: string) {
  const parsed = Date.parse(iso);
  if (Number.isNaN(parsed)) return "";
  const minutes = Math.round((Date.now() - parsed) / 60000);
  if (minutes < 1) return t("ora");
  if (minutes < 60) return t("{0} min", minutes);
  if (minutes < 60 * 24) return t("{0} h", Math.round(minutes / 60));
  return t("{0} g", Math.round(minutes / 1440));
}

/**
 * RTL (Right-to-Left) utilities for handling text direction in the application.
 *
 * These utilities help manage RTL languages like Arabic, Hebrew, Persian, and Urdu.
 * They work with the i18n system to automatically update HTML attributes when
 * the language changes.
 */
import { LANGUAGE_METADATA } from "@/i18n/languages";

/**
 * Check if a language code is RTL (Right-to-Left)
 * @param langCode - The language code (e.g., 'ar', 'en', 'he')
 * @returns true if the language is RTL, false otherwise
 */
export const isRTLLanguage = (langCode: string): boolean => {
  if (!langCode) return false;
  const code = langCode.split("-")[0].toLowerCase();
  return LANGUAGE_METADATA[code]?.direction === "rtl";
};

/**
 * Get the text direction ('ltr' or 'rtl') for a language
 * @param langCode - The language code (e.g., 'ar', 'en', 'he')
 * @returns 'rtl' if RTL language, 'ltr' otherwise
 */
export const getLanguageDirection = (langCode: string): "ltr" | "rtl" => {
  return isRTLLanguage(langCode) ? "rtl" : "ltr";
};

/**
 * Update the HTML document's dir attribute
 * @param dir - The direction ('ltr' or 'rtl')
 */
export const updateDocumentDirection = (dir: "ltr" | "rtl"): void => {
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute("dir", dir);
  }
};

/**
 * Update the HTML document's lang attribute
 * @param lang - The language code (e.g., 'ar', 'en')
 */
export const updateDocumentLanguage = (lang: string): void => {
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute("lang", lang);
  }
};

/**
 * Detect text direction from actual content by looking for RTL script characters.
 * Returns "rtl" if the first strong directional character is RTL, otherwise "ltr".
 */
export const detectTextDirection = (text: string): "ltr" | "rtl" => {
  if (!text) return "ltr";
  // Match RTL script characters:
  // - Hebrew: \u0590-\u05FF, \uFB1D-\uFB4F
  // - Arabic: \u0600-\u06FF, \u0750-\u077F, \u08A0-\u08FF, \uFB50-\uFDFF, \uFE70-\uFEFF
  // - Syriac: \u0700-\u074F
  // - Thaana: \u0780-\u07BF
  const rtlRegex =
    /[\u0590-\u05FF\u0600-\u06FF\u0700-\u074F\u0750-\u077F\u0780-\u07BF\u08A0-\u08FF\uFB1D-\uFB4F\uFB50-\uFDFF\uFE70-\uFEFF]/;
  const ltrRegex = /[A-Za-z\u00C0-\u024F\u1E00-\u1EFF]/;

  for (const char of text) {
    if (rtlRegex.test(char)) return "rtl";
    if (ltrRegex.test(char)) return "ltr";
  }
  return "ltr";
};

/**
 * Initialize RTL support for the current document
 * Should be called when the app initializes and when language changes
 * @param langCode - The current language code
 */
export const initializeRTL = (langCode: string): void => {
  const dir = getLanguageDirection(langCode);
  updateDocumentDirection(dir);
  updateDocumentLanguage(langCode);
};

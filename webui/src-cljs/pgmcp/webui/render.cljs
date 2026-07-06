(ns pgmcp.webui.render
  "Pure transforms: remark's hast tree → hiccup, and tree-sitter capture spans →
  hiccup. No reagent/DOM; the async JS pipelines that PRODUCE the hast tree and
  the spans live in fx. Emitting hiccup (never a raw HTML string) keeps the
  no-raw-HTML architecture gate intact while letting rendered documents carry
  real reagent structure."
  (:require [clojure.string :as str]))

;; ── hast → hiccup ──────────────────────────────────────────────────────────
;; Allow-list of hast element tagNames. Unknown tags render their children only
;; (sanitize by omission); no raw HTML is injected, embedded HTML is dropped.

(def allowed-tags
  #{"p" "h1" "h2" "h3" "h4" "h5" "h6" "ul" "ol" "li" "blockquote"
    "pre" "code" "em" "strong" "del" "hr" "br" "a" "img"
    "table" "thead" "tbody" "tr" "th" "td" "input"})

(defn element-props [tag ^js props]
  (when props
    (let [href (.-href props)
          src (.-src props)
          alt (.-alt props)
          checked (.-checked props)]
      (cond-> {}
        (and (= tag "a") href) (assoc :href (str href) :target "_blank" :rel "noreferrer")
        (and (= tag "img") src) (assoc :src (str src) :alt (str (or alt "")) :loading "lazy")
        (= tag "input") (assoc :type "checkbox" :checked (boolean checked) :disabled true)))))

(defn hast->hiccup
  "Walk a hast node (JS object with .-type / .-tagName / .-children / .-value)
  into hiccup, dropping any tag outside the allow-list (its children survive)."
  [^js node]
  (case (some-> node .-type)
    "root" (into [:<>] (map hast->hiccup (array-seq (or (.-children node) #js []))))
    "text" (str (.-value node))
    "element"
    (let [tag (.-tagName node)
          children (map hast->hiccup (array-seq (or (.-children node) #js [])))]
      (if (contains? allowed-tags tag)
        (let [props (element-props tag (.-properties node))]
          (into (if (seq props) [(keyword tag) props] [(keyword tag)]) children))
        (into [:<>] children)))
    nil))

;; ── tree-sitter spans → hiccup ─────────────────────────────────────────────

(defn spans->hiccup
  "Interleave plain text and highlighted spans. spans = seq of
  {:from :to :class} character offsets into text; emits `[:<> ...strings and
  [:span.cm-* text]...]`. Overlapping captures are resolved first-wins by the
  sort (earliest start, longest span)."
  [text spans]
  (let [text (or text "")
        n (count text)
        spans (->> spans
                   (filter #(and (:class %)
                                 (number? (:from %))
                                 (number? (:to %))
                                 (< (:from %) (:to %))))
                   (sort-by (juxt :from (comp - :to))))]
    (loop [pos 0 spans spans out []]
      (if-let [{:keys [from to class]} (first spans)]
        (if (< from pos)
          (recur pos (rest spans) out)
          (recur to (rest spans)
                 (-> out
                     (conj (subs text pos (min from n)))
                     (conj [:span {:class class} (subs text from (min to n))]))))
        (into [:<>] (conj out (subs text (min pos n))))))))

;; tree-sitter capture-name → CodeMirror-style CSS class (styled in app.css
;; against the --vv-* palette). Adapted from vinary-viewer's read-only path.
(def style-map
  {"keyword" "cm-keyword" "keyword.operator" "cm-keyword" "keyword.function" "cm-keyword"
   "keyword.return" "cm-keyword" "keyword.directive" "cm-keyword" "keyword.control" "cm-keyword"
   "operator" "cm-operator" "variable" "cm-variable" "variable.parameter" "cm-variable"
   "variable.builtin" "cm-variable" "number" "cm-number" "string" "cm-string"
   "string.special" "cm-string" "string.escape" "cm-string" "character" "cm-string"
   "boolean" "cm-boolean" "comment" "cm-comment" "comment.documentation" "cm-comment"
   "type" "cm-type" "type.builtin" "cm-type" "constant" "cm-constant"
   "constant.builtin" "cm-constant" "constant.numeric" "cm-number" "function" "cm-function"
   "function.call" "cm-function" "function.builtin" "cm-function" "function.method" "cm-function-method"
   "constructor" "cm-type" "method" "cm-function-method" "property" "cm-property"
   "attribute" "cm-attribute" "annotation" "cm-annotation" "punctuation" "cm-punctuation"
   "punctuation.delimiter" "cm-delimiter" "punctuation.bracket" "cm-bracket" "tag" "cm-tag"
   "label" "cm-label" "markup.heading" "cm-md-heading" "markup.raw" "cm-md-code"
   "markup.link" "cm-md-link" "markup.strong" "cm-md-strong" "markup.italic" "cm-md-emphasis"})

(defn class-for [capture-name]
  (when-not (= "none" capture-name)
    (or (get style-map capture-name)
        (get style-map (first (str/split capture-name #"\."))))))

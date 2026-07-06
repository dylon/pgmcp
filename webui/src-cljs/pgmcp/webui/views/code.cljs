(ns pgmcp.webui.views.code
  "Read-only code view: an optional file:line/language header, a copy action,
  and a monospace body. When an :id and a supported :language are given the body
  is tree-sitter highlighted (async via :render/code → render/spans->hiccup);
  until it resolves — and for languages without a bundled grammar — plain text
  shows. Rendering is hiccup-only — no raw HTML injection."
  (:require [clojure.string :as str]
            [reagent.core :as r]
            [re-com.core :as rc]
            [re-frame.core :as rf]))

(defn header-label [{:keys [path lines language]}]
  (->> [(when-not (str/blank? (str path)) (str path))
        (when-not (str/blank? (str lines)) (str ":" lines))
        (when-not (str/blank? (str language)) language)]
       (remove nil?)
       (str/join "  ")))

(defn code-view
  "opts: {:path :lines :language :code :id}."
  [{:keys [id code language] :as opts}]
  (r/with-let [_ (when (and id (not (str/blank? (str language))))
                   (rf/dispatch [:render/code id (str code) language]))]
    (let [rendered (when id @(rf/subscribe [:render/result id]))]
      [:div.code-view
       [:div.code-header
        [:span (header-label opts)]
        [rc/button
         :class "copy-btn"
         :label "copy"
         :attr {:title "Copy to clipboard"}
         :on-click #(rf/dispatch [:runtime/copy (str code)])]]
       [:pre (or rendered (str code))]])))

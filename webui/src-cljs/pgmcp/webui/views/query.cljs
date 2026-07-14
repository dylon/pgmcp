(ns pgmcp.webui.views.query
  (:require [clojure.string :as str]
            [pgmcp.webui.domain :as domain]
            [pgmcp.webui.schema :as schema]
            [pgmcp.webui.views.code :as code]
            [pgmcp.webui.views.common :as ui]
            [re-com.core :as rc]
            [re-frame.core :as rf]))

(defn set-field! [field value]
  (rf/dispatch [:machine/dispatch {:type :query/set-field
                                    :field field
                                    :value value}]))

(defn query-form []
  (let [{:keys [mode text project limit]} @(rf/subscribe [:query/form])
        can-run? @(rf/subscribe [:query/can-run?])
        pending? @(rf/subscribe [:query/pending?])]
    [:form.querybar
     {:on-submit (fn [event]
                   (.preventDefault event)
                   (when can-run?
                     (rf/dispatch [:machine/dispatch {:type :query/run}])))}
     [ui/labeled-field "Mode"
      [rc/single-dropdown
       :class "query-mode"
       :choices schema/query-mode-choices
       :model mode
       :on-change #(set-field! :mode %)
       :width "132px"]]
     [ui/labeled-field "Query"
      [rc/input-text
       :class "query-text"
       :model text
       :placeholder "search terms / pattern"
       :change-on-blur? false
       :on-change #(set-field! :text %)]]
     [ui/labeled-field "Project"
      [rc/input-text
       :class "query-project"
       :model project
       :placeholder "all projects"
       :change-on-blur? false
       :on-change #(set-field! :project %)]]
     [ui/labeled-field "Limit"
      [rc/input-text
       :class "query-limit"
       :model (str limit)
       :placeholder "20"
       :width "82px"
       :change-on-blur? false
       :validation-regex #"^\d{0,3}$"
       :attr {:aria-label "limit"
              :inputMode "numeric"
              :pattern "[0-9]*"}
       :on-change #(set-field! :limit %)]]
     [ui/labeled-field " "
      [rc/button
       :label (if pending? "Running" "Run")
       :class "btn-primary"
       :disabled? (not can-run?)
       :attr {:type "submit"}]]]))

(defn result-row [id {:keys [path lines language project score snippet]}]
  [:div.result-row
   [ui/meta-row path (when-not (str/blank? lines) (str ":" lines)) language project score]
   (when-not (str/blank? snippet)
     [code/code-view {:id id :language language :code (ui/preview-text snippet)}])])

(defn query-results []
  (let [payload @(rf/subscribe [:query/payload])
        rows @(rf/subscribe [:query/results])
        state @(rf/subscribe [:control/query])
        pending? @(rf/subscribe [:query/pending?])
        truncated? @(rf/subscribe [:query/truncated?])]
    [:div.results
     [ui/summary-row [(name state)
                      (when pending? "loading")]]
     (cond
       (nil? payload)
       (if pending?
         [ui/skeleton-rows]
         [ui/empty-box "No query results loaded — enter a query and Run."])

       (:error payload)
       [ui/error-box (:error payload)]

       (seq rows)
       [:<>
        [ui/summary-row [(str (or (:mode payload) "query"))
                         (str (count rows) " rows")
                         (when truncated? "truncated")]]
        (for [[idx row] (map-indexed vector rows)]
          ^{:key (str (:path row) ":" (:lines row) ":" idx)}
          [result-row (str "q:" idx) row])]

       :else
       [ui/summary-row [(str (or (:mode payload) "query"))
                        "0 rows"
                        (when (domain/query-truncated? payload) "truncated")]])]))

(defn query-page []
  [ui/page
   "query-view"
   [query-form]
   [query-results]])

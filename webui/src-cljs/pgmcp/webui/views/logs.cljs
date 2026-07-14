(ns pgmcp.webui.views.logs
  "Daemon log viewer — bounded tail with level + time-range filters, plus
  liblevenshtein fuzzy grep. An empty grep query tails the log (honoring the
  level/since/until filters); a non-empty query fuzzy-greps it. `since`/`until`
  are RFC3339 and apply only under the JSON log format (each line carries a
  timestamp); level filtering works in any format."
  (:require [clojure.string :as str]
            [pgmcp.webui.domain :as domain]
            [pgmcp.webui.views.common :as ui]
            [pgmcp.webui.views.panel :as panel]
            [re-com.core :as rc]
            [re-frame.core :as rf]))

(def level-choices
  [{:id "" :label "all levels"}
   {:id "ERROR" :label "ERROR"}
   {:id "WARN" :label "WARN"}
   {:id "INFO" :label "INFO"}
   {:id "DEBUG" :label "DEBUG"}
   {:id "TRACE" :label "TRACE"}])

(defn blank->nil [s]
  (when-not (str/blank? (str s)) s))

(defn tail-url [level since until]
  (str "/api/logs/tail?lines=300"
       (when (blank->nil level) (str "&level=" (js/encodeURIComponent level)))
       (when (blank->nil since) (str "&since=" (js/encodeURIComponent since)))
       (when (blank->nil until) (str "&until=" (js/encodeURIComponent until)))))

(defn grep-url [q]
  (str "/api/logs/grep?q=" (js/encodeURIComponent q) "&distance=2&limit=200"))

(defn set-param! [key value]
  (rf/dispatch [:ui/set-panel-param :logs key value]))

(defn logs-controls [q level since until]
  [:<>
   [ui/labeled-field "Level"
    [rc/single-dropdown
     :choices level-choices
     :model (or level "")
     :width "120px"
     :on-change #(set-param! :level %)]]
   [ui/labeled-field "Since"
    [rc/input-text
     :class "query-text"
     :model (or since "")
     :placeholder "RFC3339"
     :width "170px"
     :change-on-blur? false
     :on-change #(set-param! :since %)]]
   [ui/labeled-field "Until"
    [rc/input-text
     :class "query-text"
     :model (or until "")
     :placeholder "RFC3339"
     :width "170px"
     :change-on-blur? false
     :on-change #(set-param! :until %)]]
   [ui/labeled-field "Grep"
    [rc/input-text
     :class "query-text"
     :model (or q "")
     :placeholder "fuzzy (empty = tail)"
     :change-on-blur? false
     :on-change #(set-param! :q %)]]
   [ui/labeled-field " "
    [ui/toolbar-button
     {:label "Search"
      :variant :primary
      :on-click #(panel/load! :logs (if (str/blank? (or q ""))
                                      (tail-url level since until)
                                      (grep-url q)))}]]])

(defn logs-page []
  (let [q @(rf/subscribe [:panel/ui-param :logs :q ""])
        level @(rf/subscribe [:panel/ui-param :logs :level ""])
        since @(rf/subscribe [:panel/ui-param :logs :since ""])
        until @(rf/subscribe [:panel/ui-param :logs :until ""])]
    [panel/data-panel
     {:id :logs
      :url (tail-url level since until)
      :normalizer domain/normalized-logs
      :controls [logs-controls q level since until]}]))

(ns pgmcp.webui.views.database
  "Curated read-only relational browser: an allow-listed table selector plus a
  paginated, typed rows table. No arbitrary SQL — the server validates every
  table/column/filter against its registry."
  (:require [pgmcp.webui.domain :as domain]
            [pgmcp.webui.views.common :as ui]
            [pgmcp.webui.views.panel :as panel]
            [pgmcp.webui.views.widgets :as w]
            [reagent.core :as r]
            [re-frame.core :as rf]))

(def page-size 50)

(defn rows-url [table offset]
  (str "/api/db/rows?table=" (js/encodeURIComponent table)
       "&limit=" page-size "&offset=" offset))

(defn table-selector [tables table]
  (into [:span.chips-row]
        (for [t tables]
          [ui/toolbar-button
           {:label (or (:label t) (:name t))
            :active? (= (:name t) table)
            :on-click (fn []
                        (rf/dispatch [:ui/set-panel-param :database :table (:name t)])
                        (rf/dispatch [:ui/set-panel-param :database :offset 0])
                        (panel/load! :database (rows-url (:name t) 0)))}])))

(defn pager [table offset total]
  [:span.chips-row
   [ui/toolbar-button
    {:label "‹ Prev"
     :disabled? (<= offset 0)
     :on-click (fn []
                 (let [o (max 0 (- offset page-size))]
                   (rf/dispatch [:ui/set-panel-param :database :offset o])
                   (panel/load! :database (rows-url table o))))}]
   [ui/toolbar-button
    {:label "Next ›"
     :disabled? (>= (+ offset page-size) (or total 0))
     :on-click (fn []
                 (let [o (+ offset page-size)]
                   (rf/dispatch [:ui/set-panel-param :database :offset o])
                   (panel/load! :database (rows-url table o))))}]])

(defn database-page []
  (r/with-let [_ (panel/load! :db-tables "/api/db/tables")]
    (let [tables-payload @(rf/subscribe [:panel/payload :db-tables])
          tables (:tables tables-payload)
          table @(rf/subscribe [:panel/ui-param :database :table ""])
          offset @(rf/subscribe [:panel/ui-param :database :offset 0])
          rows-payload @(rf/subscribe [:panel/payload :database])]
      [ui/page
       "database-view"
       [:div.toolbar [table-selector tables table]]
       (cond
         (nil? tables-payload) [ui/empty-box "Loading tables…"]
         (:error tables-payload) [ui/error-box (:error tables-payload)]
         (empty? tables) [ui/empty-box "No tables available."]
         :else
         [:<>
          [:div.toolbar [pager table offset (:total rows-payload)]]
          (cond
            (nil? rows-payload) [ui/empty-box "Select a table to browse its rows."]
            (:error rows-payload) [ui/error-box (:error rows-payload)]
            :else [w/sections (domain/normalized-database rows-payload)])])])))

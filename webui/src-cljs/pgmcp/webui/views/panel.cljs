(ns pgmcp.webui.views.panel
  "Generic fetch-and-render data pane, built on the shared :panel statechart
  region. A pane supplies its panel id, the URL to fetch, a pure normalizer
  (payload -> widget sections), and an optional controls toolbar. The pane
  auto-loads on mount and reloads on Refresh or when a control dispatches a new
  :panel/load. Rendering is widget sections — no JSON dump, no raw HTML."
  (:require [pgmcp.webui.views.common :as ui]
            [pgmcp.webui.views.widgets :as w]
            [reagent.core :as r]
            [re-frame.core :as rf]))

(defn load! [id url]
  (rf/dispatch [:machine/dispatch {:type :panel/load :panel id :url url}]))

(defn set-param! [id key value]
  (rf/dispatch [:ui/set-panel-param id key value]))

(defn data-panel
  "opts: {:id <panel-kw> :url <string> :normalizer (fn payload -> sections)
          :controls <optional hiccup rendered in the toolbar>}. Each pane is a
  distinct component (so navigating remounts and re-triggers the on-mount load);
  controls dispatch their own :panel/load with a recomputed URL."
  [{:keys [id url normalizer controls poll-ms]}]
  (r/with-let [_ (load! id url)
               _ (when poll-ms
                   (rf/dispatch [:poll/start id
                                 [:machine/dispatch {:type :panel/load :panel id :url url}]
                                 poll-ms]))]
    (let [payload @(rf/subscribe [:panel/payload id])
          pending? @(rf/subscribe [:panel/pending? id])]
      [ui/page
       (str (name id) "-view")
       [:div.toolbar
        [ui/toolbar-button
         {:label (if pending? "Refreshing…" "Refresh")
          :disabled? pending?
          :on-click #(load! id url)}]
        controls]
       (cond
         (and (nil? payload) pending?)
         [ui/skeleton-rows]

         (nil? payload)
         [ui/empty-box "No data yet — click Refresh to load."]

         (:error payload)
         [ui/error-box (:error payload)]

         :else
         [w/sections (normalizer payload)])])
    (finally (when poll-ms (rf/dispatch [:poll/stop id])))))

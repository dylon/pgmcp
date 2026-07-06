(ns pgmcp.webui.views.resources
  "htop/glances-style resource monitor: per-core CPU meters, system memory,
  GPU (NVML), and pgmcp's own RSS / worker-pool / DB-pool consumption. Fed by
  the gated /api/resources snapshot; auto-loads on mount and reloads on
  Refresh. Phase D makes it live via the `status` realtime topic."
  (:require [pgmcp.webui.views.common :as ui]
            [pgmcp.webui.views.widgets :as w]
            [reagent.core :as r]
            [re-frame.core :as rf]))

(defn resources-toolbar []
  (let [pending? @(rf/subscribe [:resources/pending?])]
    [:div.toolbar
     [ui/toolbar-button
      {:label (if pending? "Refreshing…" "Refresh")
       :disabled? pending?
       :on-click #(rf/dispatch [:machine/dispatch {:type :resources/load}])}]]))

(defn resources-page []
  (r/with-let [load-ev [:machine/dispatch {:type :resources/load}]
               _ (rf/dispatch load-ev)
               _ (rf/dispatch [:poll/start :resources load-ev 3000])]
    (let [payload @(rf/subscribe [:resources/payload])
          pending? @(rf/subscribe [:resources/pending?])
          sections @(rf/subscribe [:resources/normalized])]
      [ui/page
       "resources-view"
       [resources-toolbar]
       (cond
         (and (nil? payload) pending?)
         [ui/empty-box "Loading resources…"]

         (nil? payload)
         [ui/empty-box "No resource data. Click Refresh."]

         (:error payload)
         [ui/error-box (:error payload)]

         :else
         [w/sections sections])])
    (finally (rf/dispatch [:poll/stop :resources]))))

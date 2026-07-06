(ns pgmcp.webui.views.overview
  (:require [pgmcp.webui.schema :as schema]
            [pgmcp.webui.views.common :as ui]
            [pgmcp.webui.views.widgets :as w]
            [re-com.core :as rc]
            [re-frame.core :as rf]))

(defn stats-toolbar []
  (let [current @(rf/subscribe [:stats/current-kind])
        raw? @(rf/subscribe [:runtime/raw-panels?])]
    [:div.toolbar
     (into [:div.tabs]
           (mapv (fn [kind]
                   [ui/toolbar-button
                    {:label (get schema/stats-labels kind (name kind))
                     :active? (= kind current)
                     :on-click #(rf/dispatch [:machine/dispatch {:type :stats/load :kind kind}])}])
                 schema/stats-kinds))
     [rc/gap :size "1"]
     [ui/toolbar-button
      {:label (if raw? "Formatted" "Raw JSON")
       :active? raw?
       :on-click #(rf/dispatch [:runtime/toggle-raw])}]]))

(defn rejects-section [rejects]
  [w/section
   {:title (str "Machine rejects (" (count rejects) ")")
    :table {:columns [{:key :at :label "At"}
                      {:key :event :label "Event"}
                      {:key :reason :label "Reason"}]
            :rows (mapv (fn [r] {:at (str (:at r))
                                 :event (str (:event r))
                                 :reason (str (:reason r))})
                        rejects)}}])

(defn overview-page []
  (let [kind @(rf/subscribe [:stats/current-kind])
        payload @(rf/subscribe [:stats/current-payload])
        pending? @(rf/subscribe [:stats/current-pending?])
        raw? @(rf/subscribe [:runtime/raw-panels?])
        sections @(rf/subscribe [:stats/normalized])
        rejects @(rf/subscribe [:machine/rejects])]
    [ui/page
     "overview-view"
     [stats-toolbar]
     (cond
       (:error payload)
       [ui/error-box (:error payload)]

       (or (nil? payload) (:pending payload))
       [ui/empty-box (if pending? "Loading…" "No data loaded.")]

       raw?
       [:div.grid [ui/panel (get schema/stats-labels kind (name kind)) payload]]

       :else
       [w/sections sections])
     (when (seq rejects)
       [rejects-section rejects])]))

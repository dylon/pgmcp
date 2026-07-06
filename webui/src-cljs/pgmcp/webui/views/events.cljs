(ns pgmcp.webui.views.events
  (:require [pgmcp.webui.schema :as schema]
            [pgmcp.webui.views.common :as ui]
            [re-com.core :as rc]
            [re-frame.core :as rf]))

(defn topic-checkbox [{:keys [id label checked? disabled?]}]
  [rc/checkbox
   :class "topic-choice"
   :label label
   :model checked?
   :disabled? disabled?
   :on-change #(rf/dispatch [:machine/dispatch
                             {:type :events/topic
                              :topic id
                              :checked? %}])])

(defn topic-filter []
  (let [topics @(rf/subscribe [:events/topics])]
    [:div.topic-filter
     (for [topic topics]
       ^{:key (:id topic)}
       [topic-checkbox topic])]))

(defn event-actions []
  (let [paused? @(rf/subscribe [:events/paused?])]
    [rc/h-box
     :class "event-actions"
     :gap "8px"
     :children [[rc/button
                 :label (if paused? "Resume" "Pause")
                 :class (when paused? "active")
                 :on-click #(rf/dispatch [:machine/dispatch {:type :events/pause}])]
                [rc/button
                 :label "Clear"
                 :on-click #(rf/dispatch [:machine/dispatch {:type :events/clear}])]]]))

(defn event-toolbar []
  [:div.event-toolbar
   [topic-filter]
   [event-actions]])

(defn event-summary []
  (let [{:keys [applied-seq server-seq visible-count queued-count topic-counts]}
        @(rf/subscribe [:events/summary])]
    [ui/summary-row
     (concat [(str "seq " applied-seq "/" server-seq)
              (str visible-count " shown")
              (when (pos? queued-count) (str queued-count " queued"))]
             (map (fn [[topic n]] (str topic ":" n))
                  (sort-by first topic-counts)))]))

(defn payload-preview [payload]
  (when (and payload (not= payload {}))
    (ui/json-preview payload)))

(defn event-row [event]
  (let [preview (payload-preview (:payload event))]
    [:div.event-row
     [:code (str "#" (or (:seq event) ""))]
     [:span (schema/topic-label (:topic event))]
     [:span
      [:strong (or (:entity_kind event) "")]
      ":"
      (or (:entity_id event) "")
      " "
      (or (:op event) "")
      (when preview
        [:small preview])]]))

(defn event-log []
  (let [events @(rf/subscribe [:events/visible])]
    [:div.log
     (if (seq events)
       (for [[idx event] (map-indexed vector events)]
         ^{:key (str (:seq event) ":" idx)}
         [event-row event])
       [ui/empty-box "No realtime events received."])]))

(defn events-page []
  [ui/page
   "events-view"
   [event-toolbar]
   [event-summary]
   [event-log]])

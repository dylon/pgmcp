(ns pgmcp.webui.subs
  (:require [pgmcp.webui.domain :as domain]
            [pgmcp.webui.schema :as schema]
            [re-frame.core :as rf]))

(rf/reg-sub
 :machine
 (fn [db _]
   (:machine db)))

(rf/reg-sub
 :store
 :<- [:machine]
 (fn [machine _]
   (:s machine)))

(rf/reg-sub
 :runtime/token
 (fn [db _]
   (get-in db [:runtime :token] "")))

(rf/reg-sub
 :control/view
 :<- [:store]
 (fn [store _]
   (get-in store [:control :view])))

(rf/reg-sub
 :control/connection
 :<- [:store]
 (fn [store _]
   (get-in store [:control :connection])))

(rf/reg-sub
 :control/activity
 :<- [:store]
 (fn [store _]
   (get-in store [:control :activity])))

(rf/reg-sub
 :control/query
 :<- [:store]
 (fn [store _]
   (get-in store [:control :query])))

(rf/reg-sub
 :control/mandates
 :<- [:store]
 (fn [store _]
   (get-in store [:control :mandates])))

(rf/reg-sub
 :control/work
 :<- [:store]
 (fn [store _]
   (get-in store [:control :work])))

(rf/reg-sub
 :control/events
 :<- [:store]
 (fn [store _]
   (get-in store [:control :events])))

(rf/reg-sub
 :control/session
 :<- [:store]
 (fn [store _]
   (get-in store [:control :session])))

(rf/reg-sub
 :session/message
 :<- [:store]
 (fn [store _]
   (get-in store [:ui :session :message])))

(rf/reg-sub
 :runtime/theme
 (fn [db _]
   (get-in db [:runtime :theme] :dark)))

(rf/reg-sub
 :requests/pending?
 :<- [:store]
 (fn [store _]
   (domain/any-request-pending? store)))

(rf/reg-sub
 :stats/current-kind
 :<- [:store]
 (fn [store _]
   (get-in store [:ui :stats-kind])))

(rf/reg-sub
 :stats/current-payload
 :<- [:store]
 :<- [:stats/current-kind]
 (fn [[store kind] _]
   (or (get-in store [:domain :stats kind]) {:pending true})))

(rf/reg-sub
 :stats/current-pending?
 :<- [:store]
 :<- [:stats/current-kind]
 (fn [[store kind] _]
   (domain/request-pending? store :stats kind)))

(rf/reg-sub
 :stats/normalized
 :<- [:stats/current-kind]
 :<- [:stats/current-payload]
 (fn [[kind payload] _]
   (domain/normalized-stats kind payload)))

(rf/reg-sub
 :runtime/raw-panels?
 (fn [db _]
   (boolean (get-in db [:runtime :raw-panels?]))))

(rf/reg-sub
 :query/form
 :<- [:store]
 (fn [store _]
   (get-in store [:ui :query])))

(rf/reg-sub
 :query/payload
 :<- [:store]
 (fn [store _]
   (get-in store [:domain :query-result])))

(rf/reg-sub
 :query/pending?
 :<- [:store]
 :<- [:control/query]
 (fn [[store state] _]
   (and (= :submitted state)
        (domain/request-pending? store :query))))

(rf/reg-sub
 :query/runnable?
 :<- [:store]
 (fn [store _]
   (domain/query-runnable? store)))

(rf/reg-sub
 :query/can-run?
 :<- [:query/runnable?]
 :<- [:query/pending?]
 (fn [[runnable? pending?] _]
   (and runnable? (not pending?))))

(rf/reg-sub
 :query/results
 :<- [:query/payload]
 (fn [payload _]
   (domain/normalized-query-rows payload)))

(rf/reg-sub
 :query/truncated?
 :<- [:query/payload]
 (fn [payload _]
   (domain/query-truncated? payload)))

(rf/reg-sub
 :events/paused?
 :<- [:control/events]
 (fn [state _]
   (= :paused state)))

(rf/reg-sub
 :events/topics
 :<- [:store]
 (fn [store _]
   (let [selected-count (count (domain/selected-topics store))]
     (mapv (fn [topic]
             (let [checked? (domain/topic-selected? store topic)]
               {:id topic
                :label (schema/topic-label topic)
                :checked? checked?
                :disabled? (and checked? (= selected-count 1))}))
           schema/topics))))

(rf/reg-sub
 :events/visible
 :<- [:store]
 (fn [store _]
   (domain/visible-events store)))

(rf/reg-sub
 :events/summary
 :<- [:store]
 (fn [store _]
   (domain/event-summary store)))

(rf/reg-sub
 :mandates/form
 :<- [:store]
 (fn [store _]
   (get-in store [:ui :mandates])))

(rf/reg-sub
 :mandates/payload
 :<- [:store]
 (fn [store _]
   (get-in store [:domain :mandates-result])))

(rf/reg-sub
 :mandates/pending?
 :<- [:store]
 :<- [:control/mandates]
 (fn [[store state] _]
   (and (= :loading state)
        (domain/request-pending? store :mandates))))

(rf/reg-sub
 :mandates/sources
 :<- [:mandates/payload]
 (fn [payload _]
   (domain/mandate-sources payload)))

(rf/reg-sub
 :work/form
 :<- [:store]
 (fn [store _]
   (get-in store [:ui :work])))

(rf/reg-sub
 :work/payload
 :<- [:store]
 (fn [store _]
   (get-in store [:domain :work-result])))

(rf/reg-sub
 :work/pending?
 :<- [:store]
 :<- [:control/work]
 (fn [[store state] _]
   (and (= :loading state)
        (domain/request-pending? store :work))))

(rf/reg-sub
 :work/items
 :<- [:work/payload]
 (fn [payload _]
   (domain/normalized-work-rows payload)))

(rf/reg-sub
 :control/resources
 :<- [:store]
 (fn [store _]
   (get-in store [:control :resources])))

(rf/reg-sub
 :resources/payload
 :<- [:store]
 (fn [store _]
   (get-in store [:domain :resources-result])))

(rf/reg-sub
 :resources/pending?
 :<- [:store]
 :<- [:control/resources]
 (fn [[store state] _]
   (and (= :loading state)
        (domain/request-pending? store :resources))))

(rf/reg-sub
 :resources/normalized
 :<- [:resources/payload]
 (fn [payload _]
   (domain/normalized-resources payload)))

(rf/reg-sub
 :control/panel
 :<- [:store]
 (fn [store _]
   (get-in store [:control :panel])))

(rf/reg-sub
 :panel/payload
 (fn [db [_ id]]
   (get-in db [:machine :s :domain :panels id :result])))

(rf/reg-sub
 :panel/pending?
 (fn [db [_ id]]
   (domain/request-pending? (get-in db [:machine :s]) :panels id)))

(rf/reg-sub
 :panel/ui-param
 (fn [db [_ id key default]]
   (get-in db [:runtime :panel-ui id key] default)))

(rf/reg-sub
 :render/result
 (fn [db [_ id]]
   (get-in db [:runtime :renders id])))

(rf/reg-sub
 :editor/save-status
 (fn [db [_ id]]
   (get-in db [:runtime :saves id])))

(rf/reg-sub
 :action/status
 (fn [db [_ key]]
   (get-in db [:runtime :actions key])))

(rf/reg-sub
 :machine/rejects
 :<- [:store]
 (fn [store _]
   (get-in store [:rings :rejects])))

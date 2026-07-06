(ns pgmcp.webui.machine-test
  (:require [cljs.test :refer [deftest is testing]]
            [clojure.string :as str]
            [pgmcp.webui.domain :as domain]
            [pgmcp.webui.machine :as machine]
            [pgmcp.webui.model :as model]
            [pgmcp.webui.schema :as schema]
            [pgmcp.webui.views.common :as ui]))

(defn event [seq topic]
  {:seq seq
   :topic topic
   :entity_kind "item"
   :entity_id (str topic "-" seq)
   :op "upsert"
   :payload {:seq seq}})

(defn with-query-text [machine text]
  (assoc-in machine [:s :ui :query :text] text))

(deftest topic-filter-preserves-at-least-one-selected-topic
  (let [store (assoc-in (model/initial-store)
                        [:ui :topics]
                        (assoc (zipmap schema/topics (repeat false)) :tracker true))]
    (testing "unchecking the last selected topic is ignored"
      (is (= store (domain/selectable-topic-change store :tracker false))))
    (testing "unknown topics are ignored"
      (is (= store (domain/selectable-topic-change store :unknown false))))))

(deftest per-topic-watermarks-preserve-replay-after-filter-changes
  (let [initial (assoc-in (model/initial-store) [:domain :applied-seq] 10)
        without-cron (domain/selectable-topic-change initial :cron false)
        with-tracker-event (domain/receive-event without-cron (event 11 :tracker))
        with-cron-again (domain/selectable-topic-change with-tracker-event :cron true)]
    (is (= 10 (get-in without-cron [:domain :topic-seqs :cron])))
    (is (= 11 (get-in with-tracker-event [:domain :topic-seqs :tracker])))
    (is (= 10 (domain/subscription-since with-cron-again)))
    (is (domain/event-fresh? with-cron-again (event 11 :cron)))
    (is (not (domain/event-fresh? with-cron-again (event 11 :tracker))))))

(deftest paused-events-are-bounded-and-drained-in-order
  (let [paused (assoc-in (model/initial-store) [:control :events] :paused)
        filled (reduce domain/receive-event paused (map #(event % :tracker) (range 1 206)))
        drained (domain/drain-queued-events filled)]
    (is (= 205 (get-in filled [:domain :applied-seq])))
    (is (empty? (get-in filled [:rings :events])))
    (is (= schema/max-events (count (get-in filled [:rings :queued-events]))))
    (is (= 205 (:seq (first (get-in filled [:rings :queued-events])))))
    (is (= schema/max-events (count (get-in drained [:rings :events]))))
    (is (empty? (get-in drained [:rings :queued-events])))
    (is (= 205 (:seq (first (get-in drained [:rings :events])))))))

(deftest duplicate-replay-frames-are-idempotent
  (let [store (domain/receive-event (model/initial-store) (event 5 :tracker))
        duplicate-frame {:type "event" :event (event 5 :tracker)}
        after-duplicate (domain/apply-frame store duplicate-frame)]
    (is (= 5 (get-in after-duplicate [:domain :applied-seq])))
    (is (= 1 (count (get-in after-duplicate [:rings :events]))))))

(deftest websocket-error-then-close-is-modeled-not-rejected
  (let [machine-0 (model/initial-machine)
        machine-1 (:machine (machine/run machine-0 {:type :ws/connect :at 1}))
        machine-2 (:machine (machine/run machine-1 {:type :ws/error :at 2}))
        machine-3 (:machine (machine/run machine-2 {:type :ws/closed :at 3}))]
    (is (= :connecting (get-in machine-1 [:s :control :connection])))
    (is (= :error (get-in machine-2 [:s :control :connection])))
    (is (= :closed (get-in machine-3 [:s :control :connection])))
    (is (empty? (get-in machine-3 [:s :rings :rejects])))))

(deftest repeated-websocket-errors-are-modeled-not-rejected
  (let [machine-0 (model/initial-machine)
        machine-1 (:machine (machine/run machine-0 {:type :ws/connect :at 1}))
        machine-2 (:machine (machine/run machine-1 {:type :ws/error :at 2}))
        machine-3 (:machine (machine/run machine-2 {:type :ws/error :at 3}))]
    (is (= :error (get-in machine-3 [:s :control :connection])))
    (is (empty? (get-in machine-3 [:s :rings :rejects])))))

(deftest server-sequence-frames-never-regress
  (let [store (-> (model/initial-store)
                  (assoc-in [:domain :applied-seq] 50)
                  (assoc-in [:domain :server-seq] 80))
        stale-heartbeat (domain/apply-frame store {:type "heartbeat" :server_seq 10})
        fresh-welcome (domain/apply-frame stale-heartbeat {:type "welcome" :server_seq 120})
        stale-resync (domain/apply-frame fresh-welcome {:type "resync"
                                                        :server_seq 7
                                                        :reason "stale"})]
    (is (= 80 (get-in stale-heartbeat [:domain :server-seq])))
    (is (= 120 (get-in fresh-welcome [:domain :server-seq])))
    (is (= 120 (get-in stale-resync [:domain :server-seq])))
    (is (= :error (get-in stale-resync [:control :connection])))))

(deftest unknown-events-are-observable-and-bounded
  (let [final-machine (reduce (fn [m n]
                                (:machine (machine/run m {:type :unknown/event :at n})))
                              (model/initial-machine)
                              (range 100))]
    (is (= schema/max-rejects (count (get-in final-machine [:s :rings :rejects]))))
    (is (= :unknown/event (:event (first (get-in final-machine [:s :rings :rejects])))))))

(deftest initial-control-state-declares-all-orthogonal-regions
  (is (= {:view :overview
          :connection :idle
          :activity :ready
          :query :editing
          :mandates :idle
          :work :idle
          :events :streaming
          :session :unknown
          :resources :idle
          :panel :idle}
         (get-in (model/initial-machine) [:s :control]))))

(deftest panel-region-models-per-id-fetch-lifecycle
  (let [load (machine/run (model/initial-machine)
                          {:type :panel/load :panel :metrics :url "/api/metrics" :at 1})
        request-id (:request-id (first (:fx load)))
        loaded (:machine (machine/run (:machine load)
                                      {:type :panel/loaded :panel :metrics :request-id request-id
                                       :payload {:series "tool_calls" :buckets []} :at 2}))
        other-load (machine/run loaded {:type :panel/load :panel :logs :url "/api/logs/tail" :at 3})
        other-id (:request-id (first (:fx other-load)))
        other-loaded (:machine (machine/run (:machine other-load)
                                            {:type :panel/loaded :panel :logs :request-id other-id
                                             :payload {:lines []} :at 4}))]
    (is (= [{:type :fetch-panel :panel :metrics :url "/api/metrics" :request-id request-id}]
           (:fx load)))
    (is (= :loading (get-in load [:machine :s :control :panel])))
    (is (= {:series "tool_calls" :buckets []}
           (get-in loaded [:s :domain :panels :metrics :result])))
    (is (= :loaded (get-in loaded [:s :control :panel])))
    (is (= {:lines []} (get-in other-loaded [:s :domain :panels :logs :result])))
    (is (= {:series "tool_calls" :buckets []}
           (get-in other-loaded [:s :domain :panels :metrics :result])))
    (is (empty? (get-in other-loaded [:s :rings :rejects])))))

(deftest resources-region-models-load-loaded-and-failed
  (let [load (machine/run (model/initial-machine) {:type :resources/load :at 1})
        request-id (:request-id (first (:fx load)))
        loaded (:machine (machine/run (:machine load)
                                      {:type :resources/loaded
                                       :request-id request-id
                                       :payload {:system {:cpu {:per_core_pct [10 20]}}}
                                       :at 2}))
        failed-run (machine/run loaded {:type :resources/load :at 3})
        failed-id (:request-id (first (:fx failed-run)))
        failed (:machine (machine/run (:machine failed-run)
                                      {:type :resources/error
                                       :request-id failed-id
                                       :message "failed"
                                       :at 4}))]
    (is (= [{:type :fetch-resources :request-id request-id}] (:fx load)))
    (is (= :loading (get-in load [:machine :s :control :resources])))
    (is (= :loaded (get-in loaded [:s :control :resources])))
    (is (= {:system {:cpu {:per_core_pct [10 20]}}}
           (get-in loaded [:s :domain :resources-result])))
    (is (= :failed (get-in failed [:s :control :resources])))
    (is (= {:error "failed"} (get-in failed [:s :domain :resources-result])))
    (is (empty? (get-in failed [:s :rings :rejects])))))

(deftest session-region-models-auth-lifecycle
  (let [machine-0 (model/initial-machine)
        check (machine/run machine-0 {:type :session/check :at 1})
        authenticating (:machine check)
        ok (machine/run authenticating {:type :session/ok :at 2})
        authorized (:machine ok)
        rejected (:machine (machine/run authenticating
                                        {:type :session/unauthorized
                                         :message "Invalid or missing webui token."
                                         :at 3}))
        reauth (machine/run rejected {:type :session/authenticate :at 4})
        expired (:machine (machine/run authorized
                                       {:type :session/expired
                                        :message "Session expired — re-enter the webui token."
                                        :at 5}))]
    (is (= :authenticating (get-in authenticating [:s :control :session])))
    (is (= [{:type :check-session}] (:fx check)))
    (is (= :authorized (get-in authorized [:s :control :session])))
    (is (= [{:type :start-app}] (:fx ok)))
    (is (= :unauthorized (get-in rejected [:s :control :session])))
    (is (= "Invalid or missing webui token." (get-in rejected [:s :ui :session :message])))
    (is (= :authenticating (get-in (:machine reauth) [:s :control :session])))
    (is (= "" (get-in (:machine reauth) [:s :ui :session :message])))
    (is (= [{:type :check-session}] (:fx reauth)))
    (is (= :unauthorized (get-in expired [:s :control :session])))
    (is (= "Session expired — re-enter the webui token."
           (get-in expired [:s :ui :session :message])))
    (is (empty? (get-in expired [:s :rings :rejects])))))

(deftest presentation-previews-are-hard-bounded
  (let [long-text (apply str (repeat (+ schema/max-preview-chars 50) "x"))
        preview (ui/preview-text long-text)]
    (is (= schema/max-preview-chars (count preview)))
    (is (str/includes? preview "truncated"))
    (is (< (count preview) (count long-text)))))

(deftest view-navigation-uses-a-pushdown-continuation-stack
  (let [machine-0 (model/initial-machine)
        query (:machine (machine/run machine-0 {:type :ui/view :view :query :at 1}))
        events (:machine (machine/run query {:type :ui/view :view :events :at 2}))
        duplicate-events (:machine (machine/run events {:type :ui/view :view :events :at 3}))
        back-to-query (:machine (machine/run duplicate-events {:type :nav/back :at 4}))
        back-to-overview (:machine (machine/run back-to-query {:type :nav/back :at 5}))
        root-back (:machine (machine/run back-to-overview {:type :nav/back :at 6}))
        invalid (:machine (machine/run machine-0 {:type :ui/view :view :missing :at 7}))]
    (is (= [{:kind :view :view :overview}] (:k machine-0)))
    (is (= :query (get-in query [:s :control :view])))
    (is (= [{:kind :view :view :overview}
            {:kind :view :view :query}]
           (:k query)))
    (is (= :events (get-in events [:s :control :view])))
    (is (= [{:kind :view :view :overview}
            {:kind :view :view :query}
            {:kind :view :view :events}]
           (:k events)))
    (is (= (:k events) (:k duplicate-events)))
    (is (= :query (get-in back-to-query [:s :control :view])))
    (is (= [{:kind :view :view :overview}
            {:kind :view :view :query}]
           (:k back-to-query)))
    (is (= :overview (get-in back-to-overview [:s :control :view])))
    (is (= [{:kind :view :view :overview}] (:k back-to-overview)))
    (is (= (:s back-to-overview) (:s root-back)))
    (is (= (:k back-to-overview) (:k root-back)))
    (is (= :overview (get-in invalid [:s :control :view])))
    (is (= [{:kind :view :view :overview}] (:k invalid)))
    (is (= :ui/view (:event (first (get-in invalid [:s :rings :rejects])))))
    (is (empty? (get-in root-back [:s :rings :rejects])))))

(deftest websocket-and-topic-actions-return-edge-effect-data
  (let [connect (machine/run (model/initial-machine) {:type :ws/connect :at 1})
        connected (:machine (machine/run (:machine connect) {:type :ws/open :at 2}))
        topic-sync (machine/run (assoc-in connected [:s :domain :applied-seq] 10)
                                {:type :events/topic
                                 :topic :cron
                                 :checked? false
                                 :at 3})
        topic-store (get-in topic-sync [:machine :s])
        disconnect (machine/run (:machine topic-sync) {:type :ws/disconnect :at 4})]
    (is (= :connecting (get-in connect [:machine :s :control :connection])))
    (is (= [{:type :connect-ws}] (:fx connect)))
    (is (= :live (get-in connected [:s :control :connection])))
    (is (= [{:type :sync-ws-subscription}] (:fx topic-sync)))
    (is (not (domain/topic-selected? topic-store :cron)))
    (is (= 10 (domain/subscription-since topic-store)))
    (is (= (->> schema/topics
                (remove #{:cron})
                (mapv name))
           (domain/subscription-topics topic-store)))
    (is (= :closed (get-in disconnect [:machine :s :control :connection])))
    (is (= [{:type :disconnect-ws}] (:fx disconnect)))
    (is (empty? (get-in disconnect [:machine :s :rings :rejects])))))

(deftest query-region-models-edit-submit-loaded-and-failed
  (let [run (machine/run (with-query-text (model/initial-machine) "webui")
                         {:type :query/run :at 1})
        request-id (:request-id (first (:fx run)))
        submitted (:machine run)
        edited (:machine (machine/run submitted
                                      {:type :query/set-field
                                       :field :text
                                       :value "statechart"
                                       :at 2}))
        loaded (:machine (machine/run edited
                                      {:type :query/loaded
                                       :request-id request-id
                                       :payload {:results [{:path "src/lib.rs"}]}
                                       :at 3}))
        edited-again (:machine (machine/run loaded
                                            {:type :query/set-field
                                             :field :text
                                             :value "missing"
                                             :at 4}))
        failed-run (machine/run edited-again {:type :query/run :at 5})
        failed-id (:request-id (first (:fx failed-run)))
        failed (:machine (machine/run (:machine failed-run)
                                      {:type :query/error
                                       :request-id failed-id
                                       :message "failed"
                                       :at 6}))]
    (is (= :submitted (get-in submitted [:s :control :query])))
    (is (= :submitted (get-in edited [:s :control :query])))
    (is (= :loaded (get-in loaded [:s :control :query])))
    (is (= :editing (get-in edited-again [:s :control :query])))
    (is (= :failed (get-in failed [:s :control :query])))
    (is (= {:error "failed"} (get-in failed [:s :domain :query-result])))
    (is (empty? (get-in failed [:s :rings :rejects])))))

(deftest mandates-region-models-load-loaded-and-failed
  (let [load (machine/run (model/initial-machine) {:type :mandates/load :at 1})
        request-id (:request-id (first (:fx load)))
        loaded (:machine (machine/run (:machine load)
                                      {:type :mandates/loaded
                                       :request-id request-id
                                       :payload {:mandates {:sources []}}
                                       :at 2}))
        edited (:machine (machine/run loaded
                                      {:type :mandates/set-field
                                       :field :project
                                       :value "pgmcp"
                                       :at 3}))
        failed-run (machine/run edited {:type :mandates/load :at 4})
        failed-id (:request-id (first (:fx failed-run)))
        failed (:machine (machine/run (:machine failed-run)
                                      {:type :mandates/error
                                       :request-id failed-id
                                       :message "failed"
                                       :at 5}))]
    (is (= :loading (get-in load [:machine :s :control :mandates])))
    (is (= :loaded (get-in loaded [:s :control :mandates])))
    (is (= :idle (get-in edited [:s :control :mandates])))
    (is (= :failed (get-in failed [:s :control :mandates])))
    (is (= {:error "failed"} (get-in failed [:s :domain :mandates-result])))
    (is (empty? (get-in failed [:s :rings :rejects])))))

(deftest work-region-models-load-loaded-and-failed
  (let [load (machine/run (model/initial-machine) {:type :work/load :at 1})
        request-id (:request-id (first (:fx load)))
        loaded (:machine (machine/run (:machine load)
                                      {:type :work/loaded
                                       :request-id request-id
                                       :payload {:view "next-actionable"
                                                 :items [{:public_id "WI-1"}]}
                                       :at 2}))
        edited (:machine (machine/run loaded
                                      {:type :work/set-field
                                       :field :view
                                       :value :blocked
                                       :at 3}))
        failed-run (machine/run edited {:type :work/load :at 4})
        failed-id (:request-id (first (:fx failed-run)))
        failed (:machine (machine/run (:machine failed-run)
                                      {:type :work/error
                                       :request-id failed-id
                                       :message "failed"
                                       :at 5}))]
    (is (= :loading (get-in load [:machine :s :control :work])))
    (is (= :loaded (get-in loaded [:s :control :work])))
    (is (= :idle (get-in edited [:s :control :work])))
    (is (= :failed (get-in failed [:s :control :work])))
    (is (= {:error "failed"} (get-in failed [:s :domain :work-result])))
    (is (empty? (get-in failed [:s :rings :rejects])))))

(deftest stale-mandates-completions-settle-without-overwriting-newer-results
  (let [machine-0 (model/initial-machine)
        first-run (machine/run machine-0 {:type :mandates/load :at 1})
        first-id (:request-id (first (:fx first-run)))
        second-run (machine/run (:machine first-run) {:type :mandates/load :at 2})
        second-id (:request-id (first (:fx second-run)))
        after-stale (:machine (machine/run (:machine second-run)
                                           {:type :mandates/loaded
                                            :request-id first-id
                                            :payload {:mandates {:sources [{:path "old"}]}}
                                            :at 3}))
        after-current (:machine (machine/run after-stale
                                             {:type :mandates/loaded
                                              :request-id second-id
                                              :payload {:mandates {:sources [{:path "new"}]}}
                                              :at 4}))]
    (is (nil? (get-in after-stale [:s :domain :mandates-result])))
    (is (= :loading (get-in after-stale [:s :control :activity])))
    (is (= :loading (get-in after-stale [:s :control :mandates])))
    (is (= {:mandates {:sources [{:path "new"}]}}
           (get-in after-current [:s :domain :mandates-result])))
    (is (= :ready (get-in after-current [:s :control :activity])))
    (is (= :loaded (get-in after-current [:s :control :mandates])))
    (is (empty? (get-in after-current [:s :domain :requests :pending])))
    (is (empty? (get-in after-current [:s :rings :rejects])))))

(deftest stale-work-completions-settle-without-overwriting-newer-results
  (let [machine-0 (model/initial-machine)
        first-run (machine/run machine-0 {:type :work/load :at 1})
        first-id (:request-id (first (:fx first-run)))
        second-run (machine/run (:machine first-run) {:type :work/load :at 2})
        second-id (:request-id (first (:fx second-run)))
        after-stale (:machine (machine/run (:machine second-run)
                                           {:type :work/loaded
                                            :request-id first-id
                                            :payload {:items [{:public_id "old"}]}
                                            :at 3}))
        after-current (:machine (machine/run after-stale
                                             {:type :work/loaded
                                              :request-id second-id
                                              :payload {:items [{:public_id "new"}]}
                                              :at 4}))]
    (is (nil? (get-in after-stale [:s :domain :work-result])))
    (is (= :loading (get-in after-stale [:s :control :activity])))
    (is (= :loading (get-in after-stale [:s :control :work])))
    (is (= {:items [{:public_id "new"}]}
           (get-in after-current [:s :domain :work-result])))
    (is (= :ready (get-in after-current [:s :control :activity])))
    (is (= :loaded (get-in after-current [:s :control :work])))
    (is (empty? (get-in after-current [:s :domain :requests :pending])))
    (is (empty? (get-in after-current [:s :rings :rejects])))))

(deftest events-region-controls-pause-queue-and-drain
  (let [paused (:machine (machine/run (model/initial-machine) {:type :events/pause :at 1}))
        queued (:machine (machine/run paused
                                      {:type :ws/frame
                                       :frame {:type "event"
                                               :event (event 1 :tracker)}
                                       :at 2}))
        streaming (:machine (machine/run queued {:type :events/pause :at 3}))]
    (is (= :paused (get-in paused [:s :control :events])))
    (is (= 1 (count (get-in queued [:s :rings :queued-events]))))
    (is (empty? (get-in queued [:s :rings :events])))
    (is (= :streaming (get-in streaming [:s :control :events])))
    (is (empty? (get-in streaming [:s :rings :queued-events])))
    (is (= 1 (count (get-in streaming [:s :rings :events]))))
    (is (empty? (get-in streaming [:s :rings :rejects])))))

(deftest out-of-order-rest-completions-are-modeled-not-dropped
  (let [machine-0 (model/initial-machine)
        stats-run (machine/run machine-0 {:type :stats/load :kind :status :at 1})
        stats-id (:request-id (first (:fx stats-run)))
        query-run (machine/run (with-query-text (:machine stats-run) "webui")
                               {:type :query/run :at 2})
        query-id (:request-id (first (:fx query-run)))
        machine-3 (:machine (machine/run (:machine query-run) {:type :stats/loaded
                                                               :kind :status
                                                               :request-id stats-id
                                                               :payload {:daemon {:phase "ready"}}
                                                               :at 3}))
        machine-4 (:machine (machine/run machine-3 {:type :query/loaded
                                                    :request-id query-id
                                                    :payload {:results [{:path "src/lib.rs"}]}
                                                    :at 4}))]
    (is (= :loading (get-in machine-3 [:s :control :activity])))
    (is (= :submitted (get-in machine-3 [:s :control :query])))
    (is (= :ready (get-in machine-4 [:s :control :activity])))
    (is (= :loaded (get-in machine-4 [:s :control :query])))
    (is (= {:daemon {:phase "ready"}}
           (get-in machine-4 [:s :domain :stats :status])))
    (is (= {:results [{:path "src/lib.rs"}]}
           (get-in machine-4 [:s :domain :query-result])))
    (is (empty? (get-in machine-4 [:s :rings :rejects])))))

(deftest blank-query-run-is-a-modeled-noop
  (let [run (machine/run (model/initial-machine) {:type :query/run :at 1})]
    (is (empty? (:fx run)))
    (is (= :ready (get-in run [:machine :s :control :activity])))
    (is (= :editing (get-in run [:machine :s :control :query])))
    (is (empty? (get-in run [:machine :s :domain :requests :pending])))
    (is (empty? (get-in run [:machine :s :rings :rejects])))))

(deftest request-pending-predicates-follow-current-request-ids
  (let [machine-0 (model/initial-machine)
        store-0 (:s machine-0)
        machine-1 (:machine (machine/run machine-0 {:type :stats/load
                                                    :kind :status
                                                    :at 1}))
        store-1 (:s machine-1)]
    (is (not (domain/any-request-pending? store-0)))
    (is (not (domain/request-pending? store-0 :stats :status)))
    (is (domain/any-request-pending? store-1))
    (is (domain/request-pending? store-1 :stats :status))
    (is (not (domain/request-pending? store-1 :stats :index)))))

(deftest stats-load-normalizes-kind-before-requesting-closed-surface
  (let [run (machine/run (model/initial-machine)
                         {:type :stats/load :kind :unknown :at 1})
        effect (first (:fx run))]
    (is (= :status (:kind effect)))
    (is (= :status (get-in run [:machine :s :ui :stats-kind])))
    (is (= :loading (get-in run [:machine :s :control :activity])))
    (is (domain/request-pending? (:s (:machine run)) :stats :status))
    (is (empty? (get-in run [:machine :s :rings :rejects])))))

(deftest stale-stats-completions-for-same-kind-do-not-overwrite-current-request
  (let [machine-0 (model/initial-machine)
        first-run (machine/run machine-0 {:type :stats/load :kind :status :at 1})
        first-id (:request-id (first (:fx first-run)))
        second-run (machine/run (:machine first-run) {:type :stats/load :kind :status :at 2})
        second-id (:request-id (first (:fx second-run)))
        after-stale (:machine (machine/run (:machine second-run)
                                           {:type :stats/loaded
                                            :kind :status
                                            :request-id first-id
                                            :payload {:daemon {:phase "old"}}
                                            :at 3}))
        after-current (:machine (machine/run after-stale
                                             {:type :stats/loaded
                                              :kind :status
                                              :request-id second-id
                                              :payload {:daemon {:phase "new"}}
                                              :at 4}))]
    (is (nil? (get-in after-stale [:s :domain :stats :status])))
    (is (= :loading (get-in after-stale [:s :control :activity])))
    (is (= {:daemon {:phase "new"}}
           (get-in after-current [:s :domain :stats :status])))
    (is (= :ready (get-in after-current [:s :control :activity])))
    (is (empty? (get-in after-current [:s :domain :requests :pending])))
    (is (empty? (get-in after-current [:s :rings :rejects])))))

(deftest stale-query-completions-settle-without-overwriting-newer-results
  (let [machine-0 (model/initial-machine)
        first-run (machine/run (with-query-text machine-0 "webui")
                               {:type :query/run :at 1})
        first-id (:request-id (first (:fx first-run)))
        second-run (machine/run (with-query-text (:machine first-run) "statechart")
                                {:type :query/run :at 2})
        second-id (:request-id (first (:fx second-run)))
        after-stale (:machine (machine/run (:machine second-run)
                                           {:type :query/loaded
                                            :request-id first-id
                                            :payload {:results [{:path "old"}]}
                                            :at 3}))
        after-current (:machine (machine/run after-stale
                                             {:type :query/loaded
                                              :request-id second-id
                                              :payload {:results [{:path "new"}]}
                                              :at 4}))]
    (is (nil? (get-in after-stale [:s :domain :query-result])))
    (is (= :loading (get-in after-stale [:s :control :activity])))
    (is (= :submitted (get-in after-stale [:s :control :query])))
    (is (= {:results [{:path "new"}]}
           (get-in after-current [:s :domain :query-result])))
    (is (= :ready (get-in after-current [:s :control :activity])))
    (is (= :loaded (get-in after-current [:s :control :query])))
    (is (empty? (get-in after-current [:s :domain :requests :pending])))
    (is (empty? (get-in after-current [:s :rings :rejects])))))

(deftest stale-query-errors-settle-without-overwriting-newer-results
  (let [machine-0 (model/initial-machine)
        first-run (machine/run (with-query-text machine-0 "webui")
                               {:type :query/run :at 1})
        first-id (:request-id (first (:fx first-run)))
        second-run (machine/run (with-query-text (:machine first-run) "statechart")
                                {:type :query/run :at 2})
        second-id (:request-id (first (:fx second-run)))
        after-current (:machine (machine/run (:machine second-run)
                                             {:type :query/loaded
                                              :request-id second-id
                                              :payload {:results [{:path "new"}]}
                                              :at 3}))
        after-stale-error (:machine (machine/run after-current
                                                 {:type :query/error
                                                  :request-id first-id
                                                  :message "old failed"
                                                  :at 4}))]
    (is (= {:results [{:path "new"}]}
           (get-in after-stale-error [:s :domain :query-result])))
    (is (= :ready (get-in after-stale-error [:s :control :activity])))
    (is (= :loaded (get-in after-stale-error [:s :control :query])))
    (is (empty? (get-in after-stale-error [:s :domain :requests :pending])))
    (is (empty? (get-in after-stale-error [:s :rings :rejects])))))

(deftest duplicate-completions-for-settled-requests-do-not-mutate-state
  (let [machine-0 (model/initial-machine)
        run-1 (machine/run (with-query-text machine-0 "webui")
                           {:type :query/run :at 1})
        request-id (:request-id (first (:fx run-1)))
        settled (:machine (machine/run (:machine run-1)
                                       {:type :query/loaded
                                        :request-id request-id
                                        :payload {:results [{:path "first"}]}
                                        :at 2}))
        duplicate (:machine (machine/run settled
                                         {:type :query/loaded
                                          :request-id request-id
                                          :payload {:results [{:path "duplicate"}]}
                                          :at 3}))]
    (is (= {:results [{:path "first"}]}
           (get-in duplicate [:s :domain :query-result])))
    (is (= :ready (get-in duplicate [:s :control :activity])))
    (is (= :loaded (get-in duplicate [:s :control :query])))
    (is (empty? (get-in duplicate [:s :domain :requests :pending])))
    (is (empty? (get-in duplicate [:s :rings :rejects])))))

(deftest request-shaping-stays-on-closed-surfaces
  (let [grep-store (-> (model/initial-store)
                       (assoc-in [:ui :query :mode] :grep)
                       (assoc-in [:ui :query :text] "foo")
                       (assoc-in [:ui :query :project] "pgmcp")
                       (assoc-in [:ui :query :limit] "50"))
        semantic-store (-> grep-store
                           (assoc-in [:ui :query :mode] :semantic)
                           (assoc-in [:ui :query :project] ""))
        text-store (assoc-in semantic-store [:ui :query :mode] :text)
        mandate-store (-> (model/initial-store)
                          (assoc-in [:ui :mandates :scope] :project)
                          (assoc-in [:ui :mandates :project] " pgmcp "))
        default-mandate-store (model/initial-store)
        invalid-mandate-store (assoc-in (model/initial-store)
                                        [:ui :mandates :scope]
                                        :unknown)
        work-store (-> (model/initial-store)
                       (assoc-in [:ui :work :assignee] " codex ")
                       (assoc-in [:ui :work :plan-public-id] " WI-PLAN ")
                       (assoc-in [:ui :work :limit] "999"))
        blocked-work-store (-> work-store
                               (assoc-in [:ui :work :view] :blocked)
                               (assoc-in [:ui :work :limit] ""))
        invalid-work-store (assoc-in (model/initial-store)
                                     [:ui :work :view]
                                     :unknown)]
    (is (= {:mode "grep" :limit 50 :pattern "foo" :project "pgmcp"}
           (domain/query-request grep-store)))
    (is (= {:mode "semantic" :limit 50 :query "foo"}
           (domain/query-request semantic-store)))
    (is (= {:mode "text" :limit 50 :query "foo"}
           (domain/query-request text-store)))
    (is (= {:scope "project" :project "pgmcp"}
           (domain/mandates-request mandate-store)))
    (is (= {:scope "all"}
           (domain/mandates-request default-mandate-store)))
    (is (= {:scope "all"}
           (domain/mandates-request invalid-mandate-store)))
    (is (= {:view "next-actionable"
            :limit 100
            :assignee "codex"
            :plan_public_id "WI-PLAN"}
           (domain/work-request work-store)))
    (is (= {:view "blocked"
            :limit 25
            :assignee "codex"}
           (domain/work-request blocked-work-store)))
    (is (= {:view "next-actionable" :limit 25}
           (domain/work-request invalid-work-store)))))

(deftest enriched-semantic-query-results-keep-path-project-and-lines
  (let [payload {:mode "semantic"
                 :data {:results [{:file_path "/workspace/pgmcp/src/lib.rs"
                                   :relative_path "src/lib.rs"
                                   :project_name "pgmcp"
                                   :language "rust"
                                   :start_line 10
                                   :end_line 12
                                   :similarity 0.98765
                                   :chunk "pub mod webui;"}]
                        :truncated true}}
        row (first (domain/normalized-query-rows payload))]
    (is (= "src/lib.rs" (:path row)))
    (is (= "10-12" (:lines row)))
    (is (= "pgmcp" (:project row)))
    (is (= "0.9877" (:score row)))
    (is (= "pub mod webui;" (:snippet row)))
    (is (domain/query-truncated? payload))))

(deftest work-view-normalization-keeps-tracker-fields
  (let [payload {:view "next-actionable"
                 :count 1
                 :items [{:public_id "WI-1"
                          :kind "task"
                          :status "ready"
                          :title "Build web UI"
                          :body "Use re-frame."
                          :priority 7
                          :claimed_percent 25
                          :assignee "codex"
                          :claimed_by "agent"
                          :due_at "2026-07-05T00:00:00Z"
                          :severity "high"}]}
        row (first (domain/normalized-work-rows payload))]
    (is (= "WI-1" (:public-id row)))
    (is (= "ready" (:status row)))
    (is (= "Build web UI" (:title row)))
    (is (= "7" (:priority row)))
    (is (= "25%" (:claimed-percent row)))
    (is (= "codex" (:assignee row)))
    (is (= "agent" (:claimed-by row)))
    (is (= "high" (:severity row)))))

(deftest query-limit-input-model-stays-editable-and-request-bounded
  (let [base (model/initial-store)
        runnable (domain/set-query-field base :text "webui")
        blank (domain/set-query-field base :limit "")
        noisy (domain/set-query-field base :limit "9x9")
        oversized (domain/set-query-field base :limit "999")
        zero (domain/set-query-field base :limit "0")]
    (is (not (domain/query-runnable? base)))
    (is (domain/query-runnable? runnable))
    (is (= "" (get-in blank [:ui :query :limit])))
    (is (= 10 (:limit (domain/query-request blank))))
    (is (= "99" (get-in noisy [:ui :query :limit])))
    (is (= 99 (:limit (domain/query-request noisy))))
    (is (= 100 (:limit (domain/query-request oversized))))
    (is (= 1 (:limit (domain/query-request zero))))))

(deftest mandate-source-normalization-keeps-effective-bundle-rows
  (let [payload {:mandates
                 {:sources [{:scope "project"
                             :kind "agents"
                             :path "AGENTS.md"
                             :text "project rules"}]
                  :project_override {:source_path ".pgmcp.toml"
                                     :sha256 "abc"
                                     :size_bytes 27
                                     :truncated false
                                     :text "[git]\nindex_history = true\n"}
                  :skipped_sources [{:scope "workspace"
                                     :kind "claude"
                                     :path "CLAUDE.md"
                                     :reason "too large"}]}}
        rows (domain/mandate-sources payload)]
    (is (= [:source :project-override :skipped] (mapv :row-kind rows)))
    (is (= "AGENTS.md" (:path (first rows))))
    (is (= ".pgmcp.toml" (:path (second rows))))
    (is (= "too large" (:text (nth rows 2))))))

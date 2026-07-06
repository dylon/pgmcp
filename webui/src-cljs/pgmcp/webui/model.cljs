(ns pgmcp.webui.model
  (:require [pgmcp.webui.schema :as schema]))

(def model
  {:id :pgmcp-webui
   :regions
   {:view
    {:initial :overview
     :states
     {:overview {:on {:ui/view {:target :$event/view :actions [:push-view]}}}
      :query {:on {:ui/view {:target :$event/view :actions [:push-view]}}}
      :events {:on {:ui/view {:target :$event/view :actions [:push-view]}}}
      :mandates {:on {:ui/view {:target :$event/view :actions [:push-view]}}}
      :work {:on {:ui/view {:target :$event/view :actions [:push-view]}}}
      :resources {:on {:ui/view {:target :$event/view :actions [:push-view]}}}
      :metrics {:on {:ui/view {:target :$event/view :actions [:push-view]}}}
      :clients {:on {:ui/view {:target :$event/view :actions [:push-view]}}}
      :database {:on {:ui/view {:target :$event/view :actions [:push-view]}}}
      :logs {:on {:ui/view {:target :$event/view :actions [:push-view]}}}
      :experiments {:on {:ui/view {:target :$event/view :actions [:push-view]}}}}}

    :session
    {:initial :unknown
     :states
     {:unknown {:on {:session/check {:target :authenticating :actions [:fx-check-session]}}}
      :authenticating {:on {:session/ok {:target :authorized :actions [:clear-session-message :fx-start-app]}
                            :session/unauthorized {:target :unauthorized :actions [:set-session-message]}
                            :session/check {:target :authenticating :actions [:fx-check-session]}}}
      :authorized {:on {:session/expired {:target :unauthorized :actions [:set-session-message :fx-disconnect-ws]}
                        :session/check {:target :authenticating :actions [:fx-check-session]}}}
      :unauthorized {:on {:session/authenticate {:target :authenticating :actions [:clear-session-message :fx-check-session]}
                          :session/expired {:target :unauthorized :actions [:set-session-message]}}}}}

    :connection
    {:initial :idle
     :states
     {:idle {:on {:ws/connect {:target :connecting :actions [:fx-connect-ws]}
                  :ws/disconnect {:target :closed :actions [:fx-disconnect-ws]}}}
      :connecting {:on {:ws/open {:target :live :actions []}
                        :ws/error {:target :error :actions []}
                        :ws/closed {:target :closed :actions []}
                        :ws/connect {:target :connecting :actions [:fx-connect-ws]}
                        :ws/disconnect {:target :closed :actions [:fx-disconnect-ws]}}}
      :live {:on {:ws/error {:target :error :actions []}
                  :ws/closed {:target :closed :actions []}
                  :ws/connect {:target :connecting :actions [:fx-connect-ws]}
                  :ws/disconnect {:target :closed :actions [:fx-disconnect-ws]}}}
      :closed {:on {:ws/connect {:target :connecting :actions [:fx-connect-ws]}
                    :ws/closed {:target :closed :actions []}
                    :ws/error {:target :error :actions []}
                    :ws/disconnect {:target :closed :actions [:fx-disconnect-ws]}}}
      :error {:on {:ws/connect {:target :connecting :actions [:fx-connect-ws]}
                   :ws/error {:target :error :actions []}
                   :ws/closed {:target :closed :actions []}
                   :ws/disconnect {:target :closed :actions [:fx-disconnect-ws]}}}}}

    :activity
    {:initial :ready
     :states
     {:ready {:on {:stats/load {:target :loading :actions [:set-stats-kind :fx-fetch-stats]}
                   :stats/loaded {:target :ready :actions [:set-stats-payload]}
                   :stats/error {:target :ready :actions [:set-stats-error]}}}
      :loading {:on {:stats/load {:target :loading :actions [:set-stats-kind :fx-fetch-stats]}
                     :stats/loaded {:target :ready :actions [:set-stats-payload]}
                     :stats/error {:target :ready :actions [:set-stats-error]}}}}}

    :query
    {:initial :editing
     :states
     {:editing {:on {:query/run {:target :$query/run-target :actions [:fx-fetch-query]}
                     :query/loaded {:target :$query/loaded-target :actions [:set-query-payload]}
                     :query/error {:target :$query/error-target :actions [:set-query-error]}}}
      :submitted {:on {:query/run {:target :$query/run-target :actions [:fx-fetch-query]}
                       :query/loaded {:target :$query/loaded-target :actions [:set-query-payload]}
                       :query/error {:target :$query/error-target :actions [:set-query-error]}}}
      :loaded {:on {:query/run {:target :$query/run-target :actions [:fx-fetch-query]}
                    :query/loaded {:target :$query/loaded-target :actions [:set-query-payload]}
                    :query/error {:target :$query/error-target :actions [:set-query-error]}}}
      :failed {:on {:query/run {:target :$query/run-target :actions [:fx-fetch-query]}
                    :query/loaded {:target :$query/loaded-target :actions [:set-query-payload]}
                    :query/error {:target :$query/error-target :actions [:set-query-error]}}}}}

    :mandates
    {:initial :idle
     :states
     {:idle {:on {:mandates/load {:target :loading :actions [:fx-fetch-mandates]}}}
      :loading {:on {:mandates/load {:target :loading :actions [:fx-fetch-mandates]}
                     :mandates/loaded {:target :$mandates/loaded-target :actions [:set-mandates-payload]}
                     :mandates/error {:target :$mandates/error-target :actions [:set-mandates-error]}}}
      :loaded {:on {:mandates/load {:target :loading :actions [:fx-fetch-mandates]}
                    :mandates/loaded {:target :$mandates/loaded-target :actions [:set-mandates-payload]}
                    :mandates/error {:target :$mandates/error-target :actions [:set-mandates-error]}}}
      :failed {:on {:mandates/load {:target :loading :actions [:fx-fetch-mandates]}
                    :mandates/loaded {:target :$mandates/loaded-target :actions [:set-mandates-payload]}
                    :mandates/error {:target :$mandates/error-target :actions [:set-mandates-error]}}}}}

    :work
    {:initial :idle
     :states
     {:idle {:on {:work/load {:target :loading :actions [:fx-fetch-work]}}}
      :loading {:on {:work/load {:target :loading :actions [:fx-fetch-work]}
                     :work/loaded {:target :$work/loaded-target :actions [:set-work-payload]}
                     :work/error {:target :$work/error-target :actions [:set-work-error]}}}
      :loaded {:on {:work/load {:target :loading :actions [:fx-fetch-work]}
                    :work/loaded {:target :$work/loaded-target :actions [:set-work-payload]}
                    :work/error {:target :$work/error-target :actions [:set-work-error]}}}
      :failed {:on {:work/load {:target :loading :actions [:fx-fetch-work]}
                    :work/loaded {:target :$work/loaded-target :actions [:set-work-payload]}
                    :work/error {:target :$work/error-target :actions [:set-work-error]}}}}}

    :resources
    {:initial :idle
     :states
     {:idle {:on {:resources/load {:target :loading :actions [:fx-fetch-resources]}}}
      :loading {:on {:resources/load {:target :loading :actions [:fx-fetch-resources]}
                     :resources/loaded {:target :$resources/loaded-target :actions [:set-resources-payload]}
                     :resources/error {:target :$resources/error-target :actions [:set-resources-error]}}}
      :loaded {:on {:resources/load {:target :loading :actions [:fx-fetch-resources]}
                    :resources/loaded {:target :$resources/loaded-target :actions [:set-resources-payload]}
                    :resources/error {:target :$resources/error-target :actions [:set-resources-error]}}}
      :failed {:on {:resources/load {:target :loading :actions [:fx-fetch-resources]}
                    :resources/loaded {:target :$resources/loaded-target :actions [:set-resources-payload]}
                    :resources/error {:target :$resources/error-target :actions [:set-resources-error]}}}}}

    ;; Generic data-panel lifecycle shared by all fetch-and-render read panes
    ;; (metrics, clients, database, logs, experiments). Panes are mutually
    ;; exclusive views, so one region tracks the active panel's load state;
    ;; per-panel staleness is guarded by the request ledger keyed on [:panels id].
    :panel
    {:initial :idle
     :states
     {:idle {:on {:panel/load {:target :loading :actions [:fx-fetch-panel]}}}
      :loading {:on {:panel/load {:target :loading :actions [:fx-fetch-panel]}
                     :panel/loaded {:target :$panel/loaded-target :actions [:set-panel-payload]}
                     :panel/error {:target :$panel/error-target :actions [:set-panel-error]}}}
      :loaded {:on {:panel/load {:target :loading :actions [:fx-fetch-panel]}
                    :panel/loaded {:target :$panel/loaded-target :actions [:set-panel-payload]}
                    :panel/error {:target :$panel/error-target :actions [:set-panel-error]}}}
      :failed {:on {:panel/load {:target :loading :actions [:fx-fetch-panel]}
                    :panel/loaded {:target :$panel/loaded-target :actions [:set-panel-payload]}
                    :panel/error {:target :$panel/error-target :actions [:set-panel-error]}}}}}

    :events
    {:initial :streaming
     :states
     {:streaming {:on {:events/pause {:target :paused :actions [:settle-events-toggle]}}}
      :paused {:on {:events/pause {:target :streaming :actions [:settle-events-toggle]}}}}}}

   :handlers
   {:ws/frame {:actions [:apply-ws-frame]}
    :events/clear {:actions [:clear-events]}
    :events/topic {:actions [:set-topic :fx-sync-ws-subscription]}
    :query/set-field {:actions [:set-query-field]}
    :mandates/set-field {:actions [:set-mandates-field]}
    :work/set-field {:actions [:set-work-field]}
    :nav/back {:actions [:pop-view]}}})

(defn initial-store []
  {:control {:view (get-in model [:regions :view :initial])
             :connection (get-in model [:regions :connection :initial])
             :activity (get-in model [:regions :activity :initial])
             :query (get-in model [:regions :query :initial])
             :mandates (get-in model [:regions :mandates :initial])
             :work (get-in model [:regions :work :initial])
             :events (get-in model [:regions :events :initial])
             :session (get-in model [:regions :session :initial])
             :resources (get-in model [:regions :resources :initial])
             :panel (get-in model [:regions :panel :initial])}
   :ui {:stats-kind :status
        :query {:mode :semantic
                :text ""
                :project ""
                :limit "10"}
        :mandates {:scope :all
                   :project ""}
        :work {:view :next-actionable
               :assignee ""
               :limit "25"
               :plan-public-id ""}
        :topics (zipmap schema/topics (repeat true))
        :session {:message ""}}
   :domain {:stats {}
            :query-result nil
            :mandates-result nil
            :work-result nil
            :resources-result nil
            :panels {}
            :applied-seq 0
            :server-seq 0
            :topic-seqs {}
            :requests {:next-id 0
                       :pending {}
                       :stats {}
                       :query nil
                       :mandates nil
                       :work nil
                       :resources nil
                       :panels {}}}
   :rings {:events []
           :queued-events []
           :rejects []}})

(defn initial-machine []
  {:c nil
   :e {:now :clock/now}
   :s (initial-store)
   :k [{:kind :view :view :overview}]})

(defn initial-db [token]
  {:machine (initial-machine)
   :runtime {:token (or token "")}})

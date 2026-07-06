(ns pgmcp.webui.machine
  (:require [pgmcp.webui.domain :as domain]
            [pgmcp.webui.model :as model]
            [pgmcp.webui.schema :as schema]))

(defn transition-for [region state-name event]
  (get-in region [:states state-name :on (:type event)]))

(defn push-view [machine event]
  (let [target (:view event)
        top (peek (:k machine))]
    {:machine
     (if (and target (not= (:view top) target))
       (update machine :k conj {:kind :view :view target})
       machine)}))

(defn pop-view [machine _event]
  (let [stack (:k machine)]
    (if (> (count stack) 1)
      (let [new-stack (pop stack)
            top (peek new-stack)]
        {:machine
         (if (and (= :view (:kind top))
                  (contains? (get-in model/model [:regions :view :states]) (:view top)))
           (-> machine
               (assoc :k new-stack)
               (assoc-in [:s :control :view] (:view top)))
           (assoc machine :k new-stack))})
      {:machine machine})))

(defn set-stats-kind [machine event]
  {:machine (assoc-in machine [:s :ui :stats-kind]
                      (schema/normalize-stats-kind (:kind event)))})

(defn sync-activity [machine]
  (assoc-in machine [:s :control :activity]
            (if (seq (get-in machine [:s :domain :requests :pending]))
              :loading
              :ready)))

(defn current-request? [machine current-path event]
  (let [request-id (:request-id event)]
    (if (some? request-id)
      (and (= request-id (get-in machine (into [:s :domain :requests] current-path)))
           (contains? (get-in machine [:s :domain :requests :pending]) request-id))
      (not (seq (get-in machine [:s :domain :requests :pending]))))))

(defn resolve-target [machine region target event fallback]
  (let [resolved (case target
                   :$event/view (:view event)
                   :$query/run-target (if (domain/query-runnable? (:s machine))
                                        :submitted
                                        fallback)
                   :$query/loaded-target (if (current-request? machine [:query] event)
                                           :loaded
                                           fallback)
                   :$query/error-target (if (current-request? machine [:query] event)
                                          :failed
                                          fallback)
                   :$mandates/loaded-target (if (current-request? machine [:mandates] event)
                                              :loaded
                                              fallback)
                   :$mandates/error-target (if (current-request? machine [:mandates] event)
                                             :failed
                                             fallback)
                   :$work/loaded-target (if (current-request? machine [:work] event)
                                           :loaded
                                           fallback)
                   :$work/error-target (if (current-request? machine [:work] event)
                                          :failed
                                          fallback)
                   :$resources/loaded-target (if (current-request? machine [:resources] event)
                                               :loaded
                                               fallback)
                   :$resources/error-target (if (current-request? machine [:resources] event)
                                              :failed
                                              fallback)
                   :$panel/loaded-target (if (current-request? machine [:panels (:panel event)] event)
                                           :loaded
                                           fallback)
                   :$panel/error-target (if (current-request? machine [:panels (:panel event)] event)
                                          :failed
                                          fallback)
                   (or target fallback))]
    (when (contains? (:states region) resolved)
      resolved)))

(defn begin-request [machine current-path]
  (let [request-id (inc (get-in machine [:s :domain :requests :next-id] 0))]
    {:machine (-> machine
                  (assoc-in [:s :domain :requests :next-id] request-id)
                  (assoc-in (into [:s :domain :requests] current-path) request-id)
                  (assoc-in [:s :domain :requests :pending request-id] true)
                  sync-activity)
     :request-id request-id}))

(defn clear-pending-request [machine event]
  (if-let [request-id (:request-id event)]
    (update-in machine [:s :domain :requests :pending] dissoc request-id)
    machine))

(defn finish-request [machine event current-path apply-fn]
  (let [apply? (current-request? machine current-path event)
        settled (clear-pending-request machine event)
        updated (if apply? (apply-fn settled) settled)]
    (sync-activity updated)))

(defn set-stats-payload [machine event]
  (let [kind (schema/normalize-stats-kind (:kind event))]
    {:machine (finish-request
               machine
               event
               [:stats kind]
               #(assoc-in % [:s :domain :stats kind] (:payload event)))}))

(defn set-stats-error [machine event]
  (let [kind (schema/normalize-stats-kind (:kind event))]
    {:machine (finish-request
               machine
               event
               [:stats kind]
               #(assoc-in % [:s :domain :stats kind] {:error (:message event)}))}))

(defn set-query-payload [machine event]
  {:machine (finish-request
             machine
             event
             [:query]
             #(assoc-in % [:s :domain :query-result] (:payload event)))})

(defn set-query-error [machine event]
  {:machine (finish-request
             machine
             event
             [:query]
             #(assoc-in % [:s :domain :query-result] {:error (:message event)}))})

(defn set-mandates-payload [machine event]
  {:machine (finish-request
             machine
             event
             [:mandates]
             #(assoc-in % [:s :domain :mandates-result] (:payload event)))})

(defn set-mandates-error [machine event]
  {:machine (finish-request
             machine
             event
             [:mandates]
             #(assoc-in % [:s :domain :mandates-result] {:error (:message event)}))})

(defn set-work-payload [machine event]
  {:machine (finish-request
             machine
             event
             [:work]
             #(assoc-in % [:s :domain :work-result] (:payload event)))})

(defn set-work-error [machine event]
  {:machine (finish-request
             machine
             event
             [:work]
             #(assoc-in % [:s :domain :work-result] {:error (:message event)}))})

(defn set-resources-payload [machine event]
  {:machine (finish-request
             machine
             event
             [:resources]
             #(assoc-in % [:s :domain :resources-result] (:payload event)))})

(defn set-resources-error [machine event]
  {:machine (finish-request
             machine
             event
             [:resources]
             #(assoc-in % [:s :domain :resources-result] {:error (:message event)}))})

(defn set-panel-payload [machine event]
  (let [id (:panel event)]
    {:machine (finish-request
               machine
               event
               [:panels id]
               #(assoc-in % [:s :domain :panels id :result] (:payload event)))}))

(defn set-panel-error [machine event]
  (let [id (:panel event)]
    {:machine (finish-request
               machine
               event
               [:panels id]
               #(assoc-in % [:s :domain :panels id :result] {:error (:message event)}))}))

(defn clear-events [machine _event]
  {:machine (-> machine
                (assoc-in [:s :rings :events] [])
                (assoc-in [:s :rings :queued-events] []))})

(defn settle-events-toggle [machine _event]
  (let [events-state (get-in machine [:s :control :events])]
    {:machine
     (if (= :streaming events-state)
       (update machine :s domain/drain-queued-events)
       machine)}))

(defn set-topic [machine event]
  {:machine (update machine :s domain/selectable-topic-change (:topic event) (:checked? event))})

(defn set-query-field [machine event]
  {:machine (cond-> (update machine :s domain/set-query-field (:field event) (:value event))
              (not (domain/request-pending? (:s machine) :query))
              (assoc-in [:s :control :query] :editing))})

(defn set-mandates-field [machine event]
  {:machine (cond-> (update machine :s domain/set-mandates-field (:field event) (:value event))
              (not (domain/request-pending? (:s machine) :mandates))
              (assoc-in [:s :control :mandates] :idle))})

(defn set-work-field [machine event]
  {:machine (cond-> (update machine :s domain/set-work-field (:field event) (:value event))
              (not (domain/request-pending? (:s machine) :work))
              (assoc-in [:s :control :work] :idle))})

(defn apply-ws-frame [machine event]
  {:machine (update machine :s domain/apply-frame (:frame event))})

(def actions
  {:push-view push-view
   :pop-view pop-view
   :set-stats-kind set-stats-kind
   :set-stats-payload set-stats-payload
   :set-stats-error set-stats-error
   :set-query-payload set-query-payload
   :set-query-error set-query-error
   :set-mandates-payload set-mandates-payload
   :set-mandates-error set-mandates-error
   :set-work-payload set-work-payload
   :set-work-error set-work-error
   :set-resources-payload set-resources-payload
   :set-resources-error set-resources-error
   :set-panel-payload set-panel-payload
   :set-panel-error set-panel-error
   :clear-events clear-events
   :settle-events-toggle settle-events-toggle
   :set-topic set-topic
   :set-query-field set-query-field
   :set-mandates-field set-mandates-field
   :set-work-field set-work-field
   :apply-ws-frame apply-ws-frame
   :set-session-message (fn [machine event]
                          {:machine (assoc-in machine [:s :ui :session :message] (or (:message event) ""))})
   :clear-session-message (fn [machine _event]
                            {:machine (assoc-in machine [:s :ui :session :message] "")})
   :fx-check-session (fn [_machine _event]
                       {:fx [{:type :check-session}]})
   :fx-start-app (fn [_machine _event]
                   {:fx [{:type :start-app}]})
   :fx-fetch-stats (fn [machine event]
                     (let [kind (schema/normalize-stats-kind (:kind event))
                           {:keys [machine request-id]} (begin-request machine [:stats kind])]
                       {:machine machine
                        :fx [{:type :fetch-stats
                              :kind kind
                              :request-id request-id}]}))
   :fx-fetch-query (fn [machine _event]
                     (if (domain/query-runnable? (:s machine))
                       (let [{:keys [machine request-id]} (begin-request machine [:query])]
                         {:machine machine
                          :fx [{:type :fetch-query
                                :request-id request-id
                                :request (domain/query-request (:s machine))}]})
                       {:machine (sync-activity machine)}))
   :fx-fetch-mandates (fn [machine _event]
                        (let [{:keys [machine request-id]} (begin-request machine [:mandates])]
                          {:machine machine
                           :fx [{:type :fetch-mandates
                                 :request-id request-id
                                 :request (domain/mandates-request (:s machine))}]}))
   :fx-fetch-work (fn [machine _event]
                    (let [{:keys [machine request-id]} (begin-request machine [:work])]
                      {:machine machine
                       :fx [{:type :fetch-work
                             :request-id request-id
                             :request (domain/work-request (:s machine))}]}))
   :fx-fetch-resources (fn [machine _event]
                         (let [{:keys [machine request-id]} (begin-request machine [:resources])]
                           {:machine machine
                            :fx [{:type :fetch-resources
                                  :request-id request-id}]}))
   :fx-fetch-panel (fn [machine event]
                     (let [id (:panel event)
                           {:keys [machine request-id]} (begin-request machine [:panels id])]
                       {:machine machine
                        :fx [{:type :fetch-panel
                              :panel id
                              :url (:url event)
                              :request-id request-id}]}))
   :fx-connect-ws (fn [_machine _event]
                    {:fx [{:type :connect-ws}]})
   :fx-disconnect-ws (fn [_machine _event]
                       {:fx [{:type :disconnect-ws}]})
   :fx-sync-ws-subscription (fn [_machine _event]
                              {:fx [{:type :sync-ws-subscription}]})})

(defn apply-actions [machine action-names event fx]
  (reduce
   (fn [{:keys [machine fx]} action-name]
     (if-let [action (get actions action-name)]
       (let [result (action machine event)]
         {:machine (or (:machine result) machine)
          :fx (if-let [new-fx (:fx result)]
                (into fx new-fx)
                fx)})
       {:machine (domain/reject machine event (str "unknown action " action-name))
        :fx fx}))
   {:machine machine :fx fx}
   action-names))

(defn step [input-machine event fx]
  (let [seed (assoc input-machine :c event)
        region-result
        (reduce
         (fn [{:keys [machine fx matched]} [region-name region]]
           (let [current-state (get-in seed [:s :control region-name])
                 transition (transition-for region current-state event)]
             (if-not transition
               {:machine machine :fx fx :matched matched}
               (let [target (resolve-target seed region (:target transition) event current-state)]
                 (if-not target
                   {:machine (domain/reject machine event (str "invalid target for " region-name))
                    :fx fx
                    :matched true}
                   (let [advanced (assoc-in machine [:s :control region-name] target)
                         applied (apply-actions advanced (:actions transition) event fx)]
                     {:machine (:machine applied)
                      :fx (:fx applied)
                      :matched true}))))))
         {:machine seed :fx fx :matched false}
         (:regions model/model))
        handler (get-in model/model [:handlers (:type event)])
        handler-result
        (if handler
          (let [applied (apply-actions (:machine region-result)
                                       (:actions handler)
                                       event
                                       (:fx region-result))]
            {:machine (:machine applied) :fx (:fx applied) :matched true})
          region-result)
        final-machine (if (:matched handler-result)
                        (:machine handler-result)
                        (domain/reject (:machine handler-result) event nil))]
    {:machine final-machine :event nil :fx (:fx handler-result)}))

(defn run [input-machine event]
  (loop [current {:machine input-machine :event event :fx []}
         steps 0]
    (if (and (:event current) (< steps 128))
      (recur (step (:machine current) (:event current) (:fx current)) (inc steps))
      (if (:event current)
        {:machine (domain/reject (:machine current) {:type :machine/trampoline-limit} nil)
         :fx (:fx current)}
        {:machine (:machine current) :fx (:fx current)}))))

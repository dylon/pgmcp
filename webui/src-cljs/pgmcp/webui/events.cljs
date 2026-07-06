(ns pgmcp.webui.events
  (:require [pgmcp.webui.domain :as domain]
            [pgmcp.webui.machine :as machine]
            [pgmcp.webui.model :as model]
            [re-frame.core :as rf]))

(defn websocket-effect-data [db]
  (let [store (get-in db [:machine :s])
        {:keys [since topics]} (domain/subscription store)]
    {:token (get-in db [:runtime :token] "")
     :since since
     :topics topics}))

(defn effect->rf [db effect]
  (let [token (get-in db [:runtime :token] "")]
    (case (:type effect)
      :fetch-stats
      [:pgmcp/fetch-stats {:kind (:kind effect)
                           :request-id (:request-id effect)
                           :token token}]

      :fetch-query
      [:pgmcp/fetch-query {:request (:request effect)
                           :request-id (:request-id effect)
                           :token token}]

      :fetch-mandates
      [:pgmcp/fetch-mandates {:request (:request effect)
                              :request-id (:request-id effect)
                              :token token}]

      :fetch-work
      [:pgmcp/fetch-work {:request (:request effect)
                          :request-id (:request-id effect)
                          :token token}]

      :fetch-resources
      [:pgmcp/fetch-resources {:request-id (:request-id effect)
                               :token token}]

      :fetch-panel
      [:pgmcp/fetch-panel {:panel (:panel effect)
                           :url (:url effect)
                           :request-id (:request-id effect)
                           :token token}]

      :check-session
      [:pgmcp/check-session {:token token}]

      :start-app
      [:pgmcp/start-app nil]

      :connect-ws
      [:pgmcp/ws-connect (websocket-effect-data db)]

      :disconnect-ws
      [:pgmcp/ws-disconnect nil]

      :sync-ws-subscription
      [:pgmcp/ws-sync-subscription (websocket-effect-data db)]

      nil)))

(defn effects->rf [db effects]
  (->> effects
       (map #(effect->rf db %))
       (remove nil?)
       vec))

(rf/reg-event-fx
 :app/init
 (fn [_ [_ {:keys [token theme]}]]
   (let [theme (or theme :dark)]
     {:db (-> (model/initial-db token)
              (assoc-in [:runtime :theme] theme))
      :fx [[:pgmcp/install-keyboard nil]
           [:pgmcp/apply-theme theme]
           ;; Validate the token before loading data; :session/ok starts the
           ;; app (stats + ws) via :fx-start-app, :session/unauthorized shows
           ;; the login overlay. When no token is configured the gated probe
           ;; returns 200 and the app starts immediately (unchanged behavior).
           [:dispatch [:machine/dispatch {:type :session/check}]]]})))

(rf/reg-event-fx
 :machine/dispatch
 [(rf/inject-cofx :pgmcp/now)]
 (fn [cofx [_ event]]
   (let [db (:db cofx)
         db (or db (model/initial-db ""))
         event (assoc event :at (:pgmcp/now cofx))
         result (machine/run (:machine db) event)
         next-db (assoc db :machine (:machine result))
         rf-effects (effects->rf next-db (:fx result))]
     (cond-> {:db next-db}
       (seq rf-effects) (assoc :fx rf-effects)))))

(rf/reg-event-fx
 :runtime/set-token
 (fn [{:keys [db]} [_ token]]
   {:db (assoc-in db [:runtime :token] (or token ""))
    :pgmcp/remember-token (or token "")}))

(rf/reg-event-fx
 :runtime/toggle-theme
 (fn [{:keys [db]} _]
   (let [next-theme (if (= :light (get-in db [:runtime :theme])) :dark :light)]
     {:db (assoc-in db [:runtime :theme] next-theme)
      :pgmcp/apply-theme next-theme})))

(rf/reg-event-db
 :runtime/toggle-raw
 (fn [db _]
   (update-in db [:runtime :raw-panels?] not)))

(rf/reg-event-fx
 :runtime/copy
 (fn [_ [_ text]]
   {:pgmcp/copy text}))

(rf/reg-event-db
 :ui/set-panel-param
 (fn [db [_ id key value]]
   (assoc-in db [:runtime :panel-ui id key] value)))

(rf/reg-event-fx
 :render/md
 (fn [_ [_ id text]]
   {:pgmcp/render-md {:id id :text text}}))

(rf/reg-event-fx
 :render/code
 (fn [_ [_ id code language]]
   {:pgmcp/highlight-code {:id id :code code :language language}}))

(rf/reg-event-db
 :render/store
 (fn [db [_ id result]]
   (assoc-in db [:runtime :renders id] result)))

(rf/reg-event-fx
 :editor/mount
 (fn [_ [_ opts]]
   {:pgmcp/editor-mount opts}))

(rf/reg-event-fx
 :editor/save
 (fn [{:keys [db]} [_ id url method]]
   {:db (assoc-in db [:runtime :saves id] :saving)
    :pgmcp/editor-save {:id id
                        :url url
                        :method method
                        :token (get-in db [:runtime :token] "")}}))

(rf/reg-event-db
 :editor/save-done
 (fn [db [_ id error]]
   (assoc-in db [:runtime :saves id] (if error {:error error} :done))))

(rf/reg-event-fx
 :poll/start
 (fn [_ [_ id event interval-ms]]
   {:pgmcp/poll-start {:id id :event event :interval-ms interval-ms}}))

(rf/reg-event-fx
 :poll/stop
 (fn [_ [_ id]]
   {:pgmcp/poll-stop {:id id}}))

(rf/reg-event-fx
 :action/submit
 (fn [{:keys [db]} [_ key {:keys [method url body on-success]}]]
   {:db (assoc-in db [:runtime :actions key] :pending)
    :pgmcp/write {:url url
                  :method method
                  :body body
                  :token (get-in db [:runtime :token] "")
                  :on-done [:action/done key on-success]}}))

(rf/reg-event-fx
 :action/done
 (fn [{:keys [db]} [_ key on-success error]]
   (cond-> {:db (assoc-in db [:runtime :actions key] (if error {:error error} :done))}
     (and (nil? error) on-success) (assoc :fx [[:dispatch on-success]]))))

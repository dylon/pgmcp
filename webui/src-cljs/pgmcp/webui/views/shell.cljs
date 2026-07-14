(ns pgmcp.webui.views.shell
  (:require [clojure.string :as str]
            [pgmcp.webui.views.clients :as clients]
            [pgmcp.webui.views.common :as common]
            [pgmcp.webui.views.database :as database]
            [pgmcp.webui.views.events :as events]
            [pgmcp.webui.views.experiments :as experiments]
            [pgmcp.webui.views.logs :as logs]
            [pgmcp.webui.views.mandates :as mandates]
            [pgmcp.webui.views.metrics :as metrics]
            [pgmcp.webui.views.overview :as overview]
            [pgmcp.webui.views.query :as query]
            [pgmcp.webui.views.resources :as resources]
            [pgmcp.webui.views.work :as work]
            [re-com.core :as rc]
            [re-frame.core :as rf]))

(def view-labels
  {:overview "Overview"
   :resources "Resources"
   :metrics "Metrics"
   :clients "Clients"
   :database "Database"
   :logs "Logs"
   :experiments "Experiments"
   :query "Query"
   :events "Events"
   :mandates "Mandates"
   :work "Work"})

;; The 11 panes clustered by intent: Workspace (what you work on) vs System
;; (how the daemon/host is doing). Rendered as two labeled segments in the tab bar.
(def nav-groups
  [{:label "Workspace" :views [:overview :query :events :work :experiments :mandates]}
   {:label "System" :views [:clients :logs :metrics :database :resources]}])

(defn brand []
  [:div.brand
   [:span.mark "pg"]
   [:span
    [:strong "pgmcp"]
    [:small "operator console"]]])

(defn nav-button [view active-view]
  [rc/button
   :label (get view-labels view (name view))
   :class (str "nav-btn" (when (= view active-view) " active"))
   :on-click #(rf/dispatch [:machine/dispatch {:type :ui/view :view view}])])

(defn navigation []
  (let [active-view @(rf/subscribe [:control/view])]
    (into [:nav.tabs]
          (for [{:keys [label views]} nav-groups]
            ^{:key label}
            [:div.nav-group
             [:span.nav-group-label label]
             (into [:div.nav-group-btns]
                   (for [v views]
                     ^{:key v} [nav-button v active-view]))]))))

(defn connection-pill []
  (let [connection @(rf/subscribe [:control/connection])]
    [:span.pill
     {:class (str (name connection) " "
                  (cond
                    (= connection :live) "live"
                    (= connection :error) "error"
                    :else ""))}
     (name connection)]))

(defn activity-pill []
  (let [activity @(rf/subscribe [:control/activity])]
    [:span.pill
     {:class (name activity)}
     (name activity)]))

(defn theme-toggle []
  (let [theme @(rf/subscribe [:runtime/theme])]
    [rc/button
     :class "theme-toggle"
     :label (if (= :light theme) "◑" "◐")
     :attr {:title "Toggle light / dark theme"}
     :on-click #(rf/dispatch [:runtime/toggle-theme])]))

(defn connection-controls []
  (let [token @(rf/subscribe [:runtime/token])]
    [:div.connection
     [rc/input-password
      :class "token-input"
      :model token
      :placeholder "token"
      :change-on-blur? false
      :attr {:autoComplete "off"
             :spellCheck false}
      :on-change #(rf/dispatch [:runtime/set-token %])]
     [rc/button
      :label "Connect"
      :on-click #(rf/dispatch [:machine/dispatch {:type :ws/connect}])]
     [rc/button
      :label "Disconnect"
      :on-click #(rf/dispatch [:machine/dispatch {:type :ws/disconnect}])]
     [connection-pill]
     [activity-pill]
     [theme-toggle]]))

(defn topbar []
  [:header.topbar
   [brand]
   [navigation]
   [connection-controls]])

(defn active-page []
  (let [active-view @(rf/subscribe [:control/view])]
    (case active-view
      :overview [overview/overview-page]
      :resources [resources/resources-page]
      :metrics [metrics/metrics-page]
      :clients [clients/clients-page]
      :database [database/database-page]
      :logs [logs/logs-page]
      :experiments [experiments/experiments-page]
      :query [query/query-page]
      :events [events/events-page]
      :mandates [mandates/mandates-page]
      :work [work/work-page]
      [overview/overview-page])))

(defn login-overlay []
  (let [token @(rf/subscribe [:runtime/token])
        message @(rf/subscribe [:session/message])]
    [:div.login-overlay
     [:div.login-card
      [:h2 "pgmcp operator console"]
      [:div.hint "This console requires the configured [webui] token. Enter it to continue."]
      (when-not (str/blank? (or message ""))
        [common/error-box message])
      [rc/input-password
       :class "login-field"
       :model token
       :placeholder "webui token"
       :change-on-blur? false
       :attr {:autoComplete "off"
              :spellCheck false}
       :on-change #(rf/dispatch [:runtime/set-token %])]
      [rc/button
       :label "Authenticate"
       :on-click #(rf/dispatch [:machine/dispatch {:type :session/authenticate}])]]]))

(defn app-root []
  (let [session @(rf/subscribe [:control/session])]
    [:div.shell
     [topbar]
     [:main [active-page]]
     (when (= :unauthorized session)
       [login-overlay])]))

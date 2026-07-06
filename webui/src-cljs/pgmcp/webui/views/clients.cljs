(ns pgmcp.webui.views.clients
  "Connected MCP clients, the projects they are editing, and their activity —
  reuses the daemon's clients stats slice (active clients + client×project
  matrix). Defaults to LIVE clients only (alive=true), polled every 5s; a
  'show exited' toggle appends ?include_exited=true to also list terminated
  clients (static — no poll). The key forces a remount on toggle so the poll
  restarts against the correct URL."
  (:require [pgmcp.webui.domain :as domain]
            [pgmcp.webui.views.panel :as panel]
            [re-com.core :as rc]
            [re-frame.core :as rf]))

(defn clients-controls [show-exited]
  [rc/checkbox
   :model show-exited
   :label "show exited"
   :on-change #(rf/dispatch [:ui/set-panel-param :clients :exited %])])

(defn clients-page []
  (let [show-exited @(rf/subscribe [:panel/ui-param :clients :exited false])]
    ^{:key (str "clients-" show-exited)}
    [panel/data-panel
     {:id :clients
      :url (str "/api/stats?kind=clients" (when show-exited "&include_exited=true"))
      :normalizer domain/normalized-clients-panel
      :poll-ms (when-not show-exited 5000)
      :controls [clients-controls show-exited]}]))

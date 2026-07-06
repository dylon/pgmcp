(ns pgmcp.webui.views.common
  (:require [clojure.string :as str]
            [pgmcp.webui.schema :as schema]
            [re-com.core :as rc]))

(def truncation-marker "\n... truncated")

(defn bounded-text [limit value]
  (let [text (str (or value ""))
        limit (max 0 limit)]
    (if (<= (count text) limit)
      text
      (let [marker (if (> (count truncation-marker) limit)
                     (subs truncation-marker 0 limit)
                     truncation-marker)
            keep (max 0 (- limit (count marker)))]
        (str (subs text 0 keep) marker)))))

(defn preview-text [value]
  (bounded-text schema/max-preview-chars value))

(defn json-text [value]
  (.stringify js/JSON (clj->js value) nil 2))

(defn json-preview [value]
  (preview-text (json-text value)))

(defn panel [title payload]
  [:article.panel
   [:h2 title]
   [:pre (json-text payload)]])

(defn empty-box [text]
  [:div.empty text])

(defn error-box [message]
  [:div.error-box (or message "Request failed")])

(defn meta-row [& items]
  [:div.meta
   (for [[idx item] (map-indexed vector (remove str/blank? (map str items)))]
     ^{:key idx} [:span item])])

(defn summary-row [items]
  [:div.summary
   (for [[idx item] (map-indexed vector (remove str/blank? (map str items)))]
     ^{:key idx} [:span item])])

(defn toolbar-button [{:keys [label active? on-click disabled?]}]
  [rc/button
   :label label
   :class (str "toolbar-button" (when active? " active"))
   :disabled? disabled?
   :on-click on-click])

(defn page [class & children]
  (into [:section {:class (str "view active " class)}] children))

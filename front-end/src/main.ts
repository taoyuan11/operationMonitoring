import { createApp } from 'vue'
import './style.css'
import App from './App.vue'
import { tooltip } from './directives/tooltip'
import { bootstrapCachedAppearance } from './utils/appearance'

bootstrapCachedAppearance()
createApp(App).directive('tooltip', tooltip).mount('#app')

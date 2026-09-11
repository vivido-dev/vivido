_vivido() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="vivido"
                ;;
            vivido,debug-bundle)
                cmd="vivido__subcmd__debug__subcmd__bundle"
                ;;
            vivido,doctor)
                cmd="vivido__subcmd__doctor"
                ;;
            vivido,help)
                cmd="vivido__subcmd__help"
                ;;
            vivido,kill-session)
                cmd="vivido__subcmd__kill__subcmd__session"
                ;;
            vivido,list)
                cmd="vivido__subcmd__list"
                ;;
            vivido,msg)
                cmd="vivido__subcmd__msg"
                ;;
            vivido__subcmd__help,debug-bundle)
                cmd="vivido__subcmd__help__subcmd__debug__subcmd__bundle"
                ;;
            vivido__subcmd__help,doctor)
                cmd="vivido__subcmd__help__subcmd__doctor"
                ;;
            vivido__subcmd__help,help)
                cmd="vivido__subcmd__help__subcmd__help"
                ;;
            vivido__subcmd__help,kill-session)
                cmd="vivido__subcmd__help__subcmd__kill__subcmd__session"
                ;;
            vivido__subcmd__help,list)
                cmd="vivido__subcmd__help__subcmd__list"
                ;;
            vivido__subcmd__help,msg)
                cmd="vivido__subcmd__help__subcmd__msg"
                ;;
            vivido__subcmd__help__subcmd__msg,capabilities)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__capabilities"
                ;;
            vivido__subcmd__help__subcmd__msg,capture)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__capture"
                ;;
            vivido__subcmd__help__subcmd__msg,config)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__config"
                ;;
            vivido__subcmd__help__subcmd__msg,create-window)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__create__subcmd__window"
                ;;
            vivido__subcmd__help__subcmd__msg,diagnose)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__diagnose"
                ;;
            vivido__subcmd__help__subcmd__msg,focus)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__focus"
                ;;
            vivido__subcmd__help__subcmd__msg,get-config)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__get__subcmd__config"
                ;;
            vivido__subcmd__help__subcmd__msg,get-grid)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__get__subcmd__grid"
                ;;
            vivido__subcmd__help__subcmd__msg,get-text)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__get__subcmd__text"
                ;;
            vivido__subcmd__help__subcmd__msg,inspect)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__inspect"
                ;;
            vivido__subcmd__help__subcmd__msg,key)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__key"
                ;;
            vivido__subcmd__help__subcmd__msg,list-windows)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__list__subcmd__windows"
                ;;
            vivido__subcmd__help__subcmd__msg,mouse)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__mouse"
                ;;
            vivido__subcmd__help__subcmd__msg,paste)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__paste"
                ;;
            vivido__subcmd__help__subcmd__msg,ping)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__ping"
                ;;
            vivido__subcmd__help__subcmd__msg,quit)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__quit"
                ;;
            vivido__subcmd__help__subcmd__msg,reset-terminal)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__reset__subcmd__terminal"
                ;;
            vivido__subcmd__help__subcmd__msg,resize)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__resize"
                ;;
            vivido__subcmd__help__subcmd__msg,restart-terminal)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__restart__subcmd__terminal"
                ;;
            vivido__subcmd__help__subcmd__msg,run-plan)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__run__subcmd__plan"
                ;;
            vivido__subcmd__help__subcmd__msg,screenshot)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__screenshot"
                ;;
            vivido__subcmd__help__subcmd__msg,set-geometry)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__set__subcmd__geometry"
                ;;
            vivido__subcmd__help__subcmd__msg,set-level)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__set__subcmd__level"
                ;;
            vivido__subcmd__help__subcmd__msg,set-visible)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__set__subcmd__visible"
                ;;
            vivido__subcmd__help__subcmd__msg,signal)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__signal"
                ;;
            vivido__subcmd__help__subcmd__msg,subscribe)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__subscribe"
                ;;
            vivido__subcmd__help__subcmd__msg,transcript)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__transcript"
                ;;
            vivido__subcmd__help__subcmd__msg,typing)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__typing"
                ;;
            vivido__subcmd__help__subcmd__msg,vivid)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__vivid"
                ;;
            vivido__subcmd__help__subcmd__msg,wait)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__wait"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__mouse,click)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__click"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__mouse,double-click)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__double__subcmd__click"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__mouse,down)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__down"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__mouse,drag)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__drag"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__mouse,move)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__move"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__mouse,path)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__path"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__mouse,scroll)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__scroll"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__mouse,up)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__up"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__vivid,scene-status)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__scene__subcmd__status"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__vivid,sessions)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__sessions"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__vivid,surface-status)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__surface__subcmd__status"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__vivid,surfaces)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__surfaces"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__vivid,trace)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__trace"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__vivid,track-status)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__track__subcmd__status"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__vivid,tracks)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__tracks"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__wait,exit)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__exit"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__wait,frame)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__frame"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__wait,output)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__output"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__wait,screen-change)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__screen__subcmd__change"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__wait,screen-stable)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__screen__subcmd__stable"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__wait,text)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__text"
                ;;
            vivido__subcmd__help__subcmd__msg__subcmd__wait,vivid-track)
                cmd="vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__vivid__subcmd__track"
                ;;
            vivido__subcmd__msg,capabilities)
                cmd="vivido__subcmd__msg__subcmd__capabilities"
                ;;
            vivido__subcmd__msg,capture)
                cmd="vivido__subcmd__msg__subcmd__capture"
                ;;
            vivido__subcmd__msg,config)
                cmd="vivido__subcmd__msg__subcmd__config"
                ;;
            vivido__subcmd__msg,create-window)
                cmd="vivido__subcmd__msg__subcmd__create__subcmd__window"
                ;;
            vivido__subcmd__msg,diagnose)
                cmd="vivido__subcmd__msg__subcmd__diagnose"
                ;;
            vivido__subcmd__msg,focus)
                cmd="vivido__subcmd__msg__subcmd__focus"
                ;;
            vivido__subcmd__msg,get-config)
                cmd="vivido__subcmd__msg__subcmd__get__subcmd__config"
                ;;
            vivido__subcmd__msg,get-grid)
                cmd="vivido__subcmd__msg__subcmd__get__subcmd__grid"
                ;;
            vivido__subcmd__msg,get-text)
                cmd="vivido__subcmd__msg__subcmd__get__subcmd__text"
                ;;
            vivido__subcmd__msg,help)
                cmd="vivido__subcmd__msg__subcmd__help"
                ;;
            vivido__subcmd__msg,inspect)
                cmd="vivido__subcmd__msg__subcmd__inspect"
                ;;
            vivido__subcmd__msg,key)
                cmd="vivido__subcmd__msg__subcmd__key"
                ;;
            vivido__subcmd__msg,list-windows)
                cmd="vivido__subcmd__msg__subcmd__list__subcmd__windows"
                ;;
            vivido__subcmd__msg,mouse)
                cmd="vivido__subcmd__msg__subcmd__mouse"
                ;;
            vivido__subcmd__msg,paste)
                cmd="vivido__subcmd__msg__subcmd__paste"
                ;;
            vivido__subcmd__msg,ping)
                cmd="vivido__subcmd__msg__subcmd__ping"
                ;;
            vivido__subcmd__msg,quit)
                cmd="vivido__subcmd__msg__subcmd__quit"
                ;;
            vivido__subcmd__msg,reset-terminal)
                cmd="vivido__subcmd__msg__subcmd__reset__subcmd__terminal"
                ;;
            vivido__subcmd__msg,resize)
                cmd="vivido__subcmd__msg__subcmd__resize"
                ;;
            vivido__subcmd__msg,restart-terminal)
                cmd="vivido__subcmd__msg__subcmd__restart__subcmd__terminal"
                ;;
            vivido__subcmd__msg,run-plan)
                cmd="vivido__subcmd__msg__subcmd__run__subcmd__plan"
                ;;
            vivido__subcmd__msg,screenshot)
                cmd="vivido__subcmd__msg__subcmd__screenshot"
                ;;
            vivido__subcmd__msg,set-geometry)
                cmd="vivido__subcmd__msg__subcmd__set__subcmd__geometry"
                ;;
            vivido__subcmd__msg,set-level)
                cmd="vivido__subcmd__msg__subcmd__set__subcmd__level"
                ;;
            vivido__subcmd__msg,set-visible)
                cmd="vivido__subcmd__msg__subcmd__set__subcmd__visible"
                ;;
            vivido__subcmd__msg,signal)
                cmd="vivido__subcmd__msg__subcmd__signal"
                ;;
            vivido__subcmd__msg,subscribe)
                cmd="vivido__subcmd__msg__subcmd__subscribe"
                ;;
            vivido__subcmd__msg,transcript)
                cmd="vivido__subcmd__msg__subcmd__transcript"
                ;;
            vivido__subcmd__msg,typing)
                cmd="vivido__subcmd__msg__subcmd__typing"
                ;;
            vivido__subcmd__msg,vivid)
                cmd="vivido__subcmd__msg__subcmd__vivid"
                ;;
            vivido__subcmd__msg,wait)
                cmd="vivido__subcmd__msg__subcmd__wait"
                ;;
            vivido__subcmd__msg__subcmd__help,capabilities)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__capabilities"
                ;;
            vivido__subcmd__msg__subcmd__help,capture)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__capture"
                ;;
            vivido__subcmd__msg__subcmd__help,config)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__config"
                ;;
            vivido__subcmd__msg__subcmd__help,create-window)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__create__subcmd__window"
                ;;
            vivido__subcmd__msg__subcmd__help,diagnose)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__diagnose"
                ;;
            vivido__subcmd__msg__subcmd__help,focus)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__focus"
                ;;
            vivido__subcmd__msg__subcmd__help,get-config)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__get__subcmd__config"
                ;;
            vivido__subcmd__msg__subcmd__help,get-grid)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__get__subcmd__grid"
                ;;
            vivido__subcmd__msg__subcmd__help,get-text)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__get__subcmd__text"
                ;;
            vivido__subcmd__msg__subcmd__help,help)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__help"
                ;;
            vivido__subcmd__msg__subcmd__help,inspect)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__inspect"
                ;;
            vivido__subcmd__msg__subcmd__help,key)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__key"
                ;;
            vivido__subcmd__msg__subcmd__help,list-windows)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__list__subcmd__windows"
                ;;
            vivido__subcmd__msg__subcmd__help,mouse)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__mouse"
                ;;
            vivido__subcmd__msg__subcmd__help,paste)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__paste"
                ;;
            vivido__subcmd__msg__subcmd__help,ping)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__ping"
                ;;
            vivido__subcmd__msg__subcmd__help,quit)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__quit"
                ;;
            vivido__subcmd__msg__subcmd__help,reset-terminal)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__reset__subcmd__terminal"
                ;;
            vivido__subcmd__msg__subcmd__help,resize)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__resize"
                ;;
            vivido__subcmd__msg__subcmd__help,restart-terminal)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__restart__subcmd__terminal"
                ;;
            vivido__subcmd__msg__subcmd__help,run-plan)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__run__subcmd__plan"
                ;;
            vivido__subcmd__msg__subcmd__help,screenshot)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__screenshot"
                ;;
            vivido__subcmd__msg__subcmd__help,set-geometry)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__set__subcmd__geometry"
                ;;
            vivido__subcmd__msg__subcmd__help,set-level)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__set__subcmd__level"
                ;;
            vivido__subcmd__msg__subcmd__help,set-visible)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__set__subcmd__visible"
                ;;
            vivido__subcmd__msg__subcmd__help,signal)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__signal"
                ;;
            vivido__subcmd__msg__subcmd__help,subscribe)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__subscribe"
                ;;
            vivido__subcmd__msg__subcmd__help,transcript)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__transcript"
                ;;
            vivido__subcmd__msg__subcmd__help,typing)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__typing"
                ;;
            vivido__subcmd__msg__subcmd__help,vivid)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__vivid"
                ;;
            vivido__subcmd__msg__subcmd__help,wait)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__wait"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__mouse,click)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__click"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__mouse,double-click)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__double__subcmd__click"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__mouse,down)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__down"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__mouse,drag)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__drag"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__mouse,move)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__move"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__mouse,path)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__path"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__mouse,scroll)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__scroll"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__mouse,up)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__up"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__vivid,scene-status)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__scene__subcmd__status"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__vivid,sessions)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__sessions"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__vivid,surface-status)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__surface__subcmd__status"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__vivid,surfaces)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__surfaces"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__vivid,trace)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__trace"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__vivid,track-status)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__track__subcmd__status"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__vivid,tracks)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__tracks"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__wait,exit)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__exit"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__wait,frame)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__frame"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__wait,output)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__output"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__wait,screen-change)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__screen__subcmd__change"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__wait,screen-stable)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__screen__subcmd__stable"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__wait,text)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__text"
                ;;
            vivido__subcmd__msg__subcmd__help__subcmd__wait,vivid-track)
                cmd="vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__vivid__subcmd__track"
                ;;
            vivido__subcmd__msg__subcmd__mouse,click)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__click"
                ;;
            vivido__subcmd__msg__subcmd__mouse,double-click)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__double__subcmd__click"
                ;;
            vivido__subcmd__msg__subcmd__mouse,down)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__down"
                ;;
            vivido__subcmd__msg__subcmd__mouse,drag)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__drag"
                ;;
            vivido__subcmd__msg__subcmd__mouse,help)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__help"
                ;;
            vivido__subcmd__msg__subcmd__mouse,move)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__move"
                ;;
            vivido__subcmd__msg__subcmd__mouse,path)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__path"
                ;;
            vivido__subcmd__msg__subcmd__mouse,scroll)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__scroll"
                ;;
            vivido__subcmd__msg__subcmd__mouse,up)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__up"
                ;;
            vivido__subcmd__msg__subcmd__mouse__subcmd__help,click)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__click"
                ;;
            vivido__subcmd__msg__subcmd__mouse__subcmd__help,double-click)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__double__subcmd__click"
                ;;
            vivido__subcmd__msg__subcmd__mouse__subcmd__help,down)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__down"
                ;;
            vivido__subcmd__msg__subcmd__mouse__subcmd__help,drag)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__drag"
                ;;
            vivido__subcmd__msg__subcmd__mouse__subcmd__help,help)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__help"
                ;;
            vivido__subcmd__msg__subcmd__mouse__subcmd__help,move)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__move"
                ;;
            vivido__subcmd__msg__subcmd__mouse__subcmd__help,path)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__path"
                ;;
            vivido__subcmd__msg__subcmd__mouse__subcmd__help,scroll)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__scroll"
                ;;
            vivido__subcmd__msg__subcmd__mouse__subcmd__help,up)
                cmd="vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__up"
                ;;
            vivido__subcmd__msg__subcmd__vivid,help)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__help"
                ;;
            vivido__subcmd__msg__subcmd__vivid,scene-status)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__scene__subcmd__status"
                ;;
            vivido__subcmd__msg__subcmd__vivid,sessions)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__sessions"
                ;;
            vivido__subcmd__msg__subcmd__vivid,surface-status)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__surface__subcmd__status"
                ;;
            vivido__subcmd__msg__subcmd__vivid,surfaces)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__surfaces"
                ;;
            vivido__subcmd__msg__subcmd__vivid,trace)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__trace"
                ;;
            vivido__subcmd__msg__subcmd__vivid,track-status)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__track__subcmd__status"
                ;;
            vivido__subcmd__msg__subcmd__vivid,tracks)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__tracks"
                ;;
            vivido__subcmd__msg__subcmd__vivid__subcmd__help,help)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__help"
                ;;
            vivido__subcmd__msg__subcmd__vivid__subcmd__help,scene-status)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__scene__subcmd__status"
                ;;
            vivido__subcmd__msg__subcmd__vivid__subcmd__help,sessions)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__sessions"
                ;;
            vivido__subcmd__msg__subcmd__vivid__subcmd__help,surface-status)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__surface__subcmd__status"
                ;;
            vivido__subcmd__msg__subcmd__vivid__subcmd__help,surfaces)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__surfaces"
                ;;
            vivido__subcmd__msg__subcmd__vivid__subcmd__help,trace)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__trace"
                ;;
            vivido__subcmd__msg__subcmd__vivid__subcmd__help,track-status)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__track__subcmd__status"
                ;;
            vivido__subcmd__msg__subcmd__vivid__subcmd__help,tracks)
                cmd="vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__tracks"
                ;;
            vivido__subcmd__msg__subcmd__wait,exit)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__exit"
                ;;
            vivido__subcmd__msg__subcmd__wait,frame)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__frame"
                ;;
            vivido__subcmd__msg__subcmd__wait,help)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__help"
                ;;
            vivido__subcmd__msg__subcmd__wait,output)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__output"
                ;;
            vivido__subcmd__msg__subcmd__wait,screen-change)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__screen__subcmd__change"
                ;;
            vivido__subcmd__msg__subcmd__wait,screen-stable)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__screen__subcmd__stable"
                ;;
            vivido__subcmd__msg__subcmd__wait,text)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__text"
                ;;
            vivido__subcmd__msg__subcmd__wait,vivid-track)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__vivid__subcmd__track"
                ;;
            vivido__subcmd__msg__subcmd__wait__subcmd__help,exit)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__exit"
                ;;
            vivido__subcmd__msg__subcmd__wait__subcmd__help,frame)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__frame"
                ;;
            vivido__subcmd__msg__subcmd__wait__subcmd__help,help)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__help"
                ;;
            vivido__subcmd__msg__subcmd__wait__subcmd__help,output)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__output"
                ;;
            vivido__subcmd__msg__subcmd__wait__subcmd__help,screen-change)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__screen__subcmd__change"
                ;;
            vivido__subcmd__msg__subcmd__wait__subcmd__help,screen-stable)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__screen__subcmd__stable"
                ;;
            vivido__subcmd__msg__subcmd__wait__subcmd__help,text)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__text"
                ;;
            vivido__subcmd__msg__subcmd__wait__subcmd__help,vivid-track)
                cmd="vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__vivid__subcmd__track"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        vivido)
            opts="-s -q -v -w -e -T -o -h -V --print-events --ref-test --config-file --socket --headless --session --automation-name --foreground --headless-size --daemon --vivid-target --window-id --no-activate --working-directory --hold --command --title --class --option --help --version msg list doctor debug-bundle kill-session help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config-file)
                    local oldifs
                    if [ -n "${IFS+x}" ]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -f "${cur}"))
                    if [ -n "${oldifs+x}" ]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                --socket)
                    local oldifs
                    if [ -n "${IFS+x}" ]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -f "${cur}"))
                    if [ -n "${oldifs+x}" ]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                -s)
                    local oldifs
                    if [ -n "${IFS+x}" ]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -f "${cur}"))
                    if [ -n "${oldifs+x}" ]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                --session)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --automation-name)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --headless-size)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --vivid-target)
                    COMPREPLY=($(compgen -W "terminal desktop" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --working-directory)
                    local oldifs
                    if [ -n "${IFS+x}" ]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -f "${cur}"))
                    if [ -n "${oldifs+x}" ]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                --command)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -e)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --title)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -T)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --class)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --option)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -o)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__debug__subcmd__bundle)
            opts="-t -h --target --output --include-screenshot --include-grid --include-transcript --include-log --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --target)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -t)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --output)
                    local oldifs
                    if [ -n "${IFS+x}" ]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -f "${cur}"))
                    if [ -n "${oldifs+x}" ]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__doctor)
            opts="-t -h --target --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --target)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -t)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help)
            opts="msg list doctor debug-bundle kill-session help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__debug__subcmd__bundle)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__doctor)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__kill__subcmd__session)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg)
            opts="create-window quit ping reset-terminal restart-terminal config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__capabilities)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__capture)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__config)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__create__subcmd__window)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__diagnose)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__focus)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__get__subcmd__config)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__get__subcmd__grid)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__get__subcmd__text)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__inspect)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__key)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__list__subcmd__windows)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__mouse)
            opts="move click double-click down up drag path scroll"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__click)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__double__subcmd__click)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__down)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__drag)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__move)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__path)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__scroll)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__mouse__subcmd__up)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__paste)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__ping)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__quit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__reset__subcmd__terminal)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__resize)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__restart__subcmd__terminal)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__run__subcmd__plan)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__screenshot)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__set__subcmd__geometry)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__set__subcmd__level)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__set__subcmd__visible)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__signal)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__subscribe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__transcript)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__typing)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__vivid)
            opts="sessions surfaces surface-status tracks track-status scene-status trace"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__scene__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__sessions)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__surface__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__surfaces)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__trace)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__track__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__vivid__subcmd__tracks)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__wait)
            opts="text output screen-change screen-stable frame vivid-track exit"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__exit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__frame)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__output)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__screen__subcmd__change)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__screen__subcmd__stable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__text)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__help__subcmd__msg__subcmd__wait__subcmd__vivid__subcmd__track)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__kill__subcmd__session)
            opts="-t -h --target --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --target)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -t)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__list)
            opts="-h --all --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg)
            opts="-s -t -h --socket --target --help create-window quit ping reset-terminal restart-terminal config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --socket)
                    local oldifs
                    if [ -n "${IFS+x}" ]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -f "${cur}"))
                    if [ -n "${oldifs+x}" ]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                -s)
                    local oldifs
                    if [ -n "${IFS+x}" ]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -f "${cur}"))
                    if [ -n "${oldifs+x}" ]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                --target)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -t)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__capabilities)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__capture)
            opts="-w -h --window-id --activate --after-frame --stable --timeout --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --after-frame)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --stable)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__config)
            opts="-w -r -h --window-id --reset --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__create__subcmd__window)
            opts="-w -e -T -o -h --vivid-target --window-id --no-activate --working-directory --hold --command --title --class --option --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --vivid-target)
                    COMPREPLY=($(compgen -W "terminal desktop" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --working-directory)
                    local oldifs
                    if [ -n "${IFS+x}" ]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -f "${cur}"))
                    if [ -n "${oldifs+x}" ]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                --command)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -e)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --title)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -T)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --class)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --option)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -o)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__diagnose)
            opts="-w -h --window-id --trace-limit --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --trace-limit)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__focus)
            opts="-w -h --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__get__subcmd__config)
            opts="-w -h --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__get__subcmd__grid)
            opts="-w -h --start-line --row-count --since-screen --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --start-line)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --row-count)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --since-screen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__get__subcmd__text)
            opts="-w -h --rows --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --rows)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help)
            opts="create-window quit ping reset-terminal restart-terminal config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__capabilities)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__capture)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__config)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__create__subcmd__window)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__diagnose)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__focus)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__get__subcmd__config)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__get__subcmd__grid)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__get__subcmd__text)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__inspect)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__key)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__list__subcmd__windows)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__mouse)
            opts="move click double-click down up drag path scroll"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__click)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__double__subcmd__click)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__down)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__drag)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__move)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__path)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__scroll)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__mouse__subcmd__up)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__paste)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__ping)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__quit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__reset__subcmd__terminal)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__resize)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__restart__subcmd__terminal)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__run__subcmd__plan)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__screenshot)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__set__subcmd__geometry)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__set__subcmd__level)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__set__subcmd__visible)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__signal)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__subscribe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__transcript)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__typing)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__vivid)
            opts="sessions surfaces surface-status tracks track-status scene-status trace"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__scene__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__sessions)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__surface__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__surfaces)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__trace)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__track__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__vivid__subcmd__tracks)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__wait)
            opts="text output screen-change screen-stable frame vivid-track exit"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__exit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__frame)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__output)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__screen__subcmd__change)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__screen__subcmd__stable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__text)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__help__subcmd__wait__subcmd__vivid__subcmd__track)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__inspect)
            opts="-w -h --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__key)
            opts="-w -h --mods --repeat --route --window-id --report --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --mods)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --repeat)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --route)
                    COMPREPLY=($(compgen -W "application ui" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__list__subcmd__windows)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse)
            opts="-h --help move click double-click down up drag path scroll help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__click)
            opts="-w -h --button --cell-column --cell-row --x --y --relative-x --relative-y --mods --route --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --button)
                    COMPREPLY=($(compgen -W "left middle right" -- "${cur}"))
                    return 0
                    ;;
                --cell-column)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cell-row)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mods)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --route)
                    COMPREPLY=($(compgen -W "application ui" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__double__subcmd__click)
            opts="-w -h --button --cell-column --cell-row --x --y --relative-x --relative-y --mods --route --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --button)
                    COMPREPLY=($(compgen -W "left middle right" -- "${cur}"))
                    return 0
                    ;;
                --cell-column)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cell-row)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mods)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --route)
                    COMPREPLY=($(compgen -W "application ui" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__down)
            opts="-w -h --button --cell-column --cell-row --x --y --relative-x --relative-y --mods --route --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --button)
                    COMPREPLY=($(compgen -W "left middle right" -- "${cur}"))
                    return 0
                    ;;
                --cell-column)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cell-row)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mods)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --route)
                    COMPREPLY=($(compgen -W "application ui" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__drag)
            opts="-w -h --button --cell-column --cell-row --x --y --relative-x --relative-y --mods --route --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --button)
                    COMPREPLY=($(compgen -W "left middle right" -- "${cur}"))
                    return 0
                    ;;
                --cell-column)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cell-row)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mods)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --route)
                    COMPREPLY=($(compgen -W "application ui" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__help)
            opts="move click double-click down up drag path scroll help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__click)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__double__subcmd__click)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__down)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__drag)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__move)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__path)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__scroll)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__help__subcmd__up)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__move)
            opts="-w -h --cell-column --cell-row --x --y --relative-x --relative-y --mods --route --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --cell-column)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cell-row)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mods)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --route)
                    COMPREPLY=($(compgen -W "application ui" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__path)
            opts="-w -h --point --button --mods --route --duration --wait-frame --timeout --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --point)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --button)
                    COMPREPLY=($(compgen -W "left middle right" -- "${cur}"))
                    return 0
                    ;;
                --mods)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --route)
                    COMPREPLY=($(compgen -W "application ui" -- "${cur}"))
                    return 0
                    ;;
                --duration)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__scroll)
            opts="-w -h --vertical --horizontal --cell-column --cell-row --x --y --relative-x --relative-y --mods --route --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --vertical)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --horizontal)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cell-column)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cell-row)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mods)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --route)
                    COMPREPLY=($(compgen -W "application ui" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__mouse__subcmd__up)
            opts="-w -h --button --cell-column --cell-row --x --y --relative-x --relative-y --mods --route --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --button)
                    COMPREPLY=($(compgen -W "left middle right" -- "${cur}"))
                    return 0
                    ;;
                --cell-column)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cell-row)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --relative-y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mods)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --route)
                    COMPREPLY=($(compgen -W "application ui" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__paste)
            opts="-w -h --route --window-id --report --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --route)
                    COMPREPLY=($(compgen -W "application ui" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__ping)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__quit)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__reset__subcmd__terminal)
            opts="-w -h --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__resize)
            opts="-w -h --columns --rows --width --height --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --columns)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --rows)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --width)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --height)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__restart__subcmd__terminal)
            opts="-w -h --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__run__subcmd__plan)
            opts="-h --file --dry-run --preflight --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --file)
                    local oldifs
                    if [ -n "${IFS+x}" ]; then
                        oldifs="$IFS"
                    fi
                    IFS=$'\n'
                    COMPREPLY=($(compgen -f "${cur}"))
                    if [ -n "${oldifs+x}" ]; then
                        IFS="$oldifs"
                    fi
                    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
                        compopt -o filenames
                    fi
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__screenshot)
            opts="-w -h --window-id --json --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__set__subcmd__geometry)
            opts="-w -h --x --y --width --height --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --x)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --y)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --width)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --height)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__set__subcmd__level)
            opts="-w -h --window-id --help normal always-on-top always-on-bottom"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__set__subcmd__visible)
            opts="-w -h --visible --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --visible)
                    COMPREPLY=($(compgen -W "true false" -- "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__signal)
            opts="-w -h --window-id --help int term hup quit tstp cont winch kill stop"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__subscribe)
            opts="-w -h --window-id --all --events --since-event --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --events)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --since-event)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__transcript)
            opts="-w -h --after-offset --max-bytes --raw --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --after-offset)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-bytes)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__typing)
            opts="-w -h --window-id --report --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid)
            opts="-h --help sessions surfaces surface-status tracks track-status scene-status trace help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__help)
            opts="sessions surfaces surface-status tracks track-status scene-status trace help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__scene__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__sessions)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__surface__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__surfaces)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__trace)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__track__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__help__subcmd__tracks)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__scene__subcmd__status)
            opts="-w -h --session-id --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --session-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__sessions)
            opts="-w -h --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__surface__subcmd__status)
            opts="-w -h --session-id --context-id --surface-id --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --session-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --context-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --surface-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__surfaces)
            opts="-w -h --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__trace)
            opts="-w -h --window-id --after --tail --before --around --preceding --following --limit --timeout --follow --session-id --context-id --surface-id --track-id --category --recovery-only --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --after)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --before)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --around)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --preceding)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --following)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --limit)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --context-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --surface-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --track-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --category)
                    COMPREPLY=($(compgen -W "connection lifecycle flow playback recovery decode render" -- "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__track__subcmd__status)
            opts="-w -h --session-id --context-id --surface-id --track-id --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --session-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --context-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --surface-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --track-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__vivid__subcmd__tracks)
            opts="-w -h --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait)
            opts="-h --help text output screen-change screen-stable frame vivid-track exit help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__exit)
            opts="-w -h --timeout --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__frame)
            opts="-w -h --after-frame --timeout --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --after-frame)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__help)
            opts="text output screen-change screen-stable frame vivid-track exit help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__exit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__frame)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__output)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__screen__subcmd__change)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__screen__subcmd__stable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__text)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__help__subcmd__vivid__subcmd__track)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__output)
            opts="-w -h --regex --base64 --after-offset --timeout --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --after-offset)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__screen__subcmd__change)
            opts="-w -h --after-screen --timeout --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --after-screen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__screen__subcmd__stable)
            opts="-w -h --quiet --after-screen --timeout --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --quiet)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --after-screen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__text)
            opts="-w -h --regex --after-screen --timeout --window-id --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --after-screen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        vivido__subcmd__msg__subcmd__wait__subcmd__vivid__subcmd__track)
            opts="-w -h --session-id --context-id --surface-id --track-id --channel-generation --value --timeout --window-id --help revision-after milestones presentation-after pts-after clock-started buffered-ended channel-accepted channel-detached track-lost"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --session-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --context-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --surface-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --track-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --channel-generation)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --value)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --window-id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -w)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _vivido -o nosort -o bashdefault -o default vivido
else
    complete -F _vivido -o bashdefault -o default vivido
fi

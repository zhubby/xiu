COPY_CONF_FILES = sh ./update_project_conf.sh

not_spport:
	echo "input make <local|online|build|clean|check>"
# build local source codes
local:
	cd ./confs && $(COPY_CONF_FILES) "local"
# pull the online crates codes and build
online:
	cd ./confs && $(COPY_CONF_FILES) "online"
check:
	cargo clippy --fix --allow-dirty --allow-no-vcs
clean:
	cargo clean
build:
	cargo build
docker:
	docker build --platform=linux/amd64 -t asyncio/xiu:0.9.1 . && docker push asyncio/xiu:0.9.1
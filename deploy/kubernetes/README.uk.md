# Sculk у Kubernetes

*[English](README.md) · Українська*

`sculk.yaml` запускає весь кластер у namespace `sculk`:

| Об'єкт | Що це |
| --- | --- |
| `StatefulSet/anvildb` + том 50 Gi | Світ: чанки, гравці, дані світу, оренди |
| `Deployment/io` (1 репліка) | Точка входу гравців |
| `Service/sculk` (LoadBalancer) | Куди підключаються гравці: 25565 TCP, і UDP для Nether Tunnel |
| `Deployment/worker` (спершу 4) | Воркери, кожен запускає ванільну гру (рушій Horizon) |
| `HorizontalPodAutoscaler/worker` | Масштабує воркери від 2 до 16 за CPU |
| `ConfigMap/sculk-settings`, `ConfigMap/sculk-config` | Налаштування кластера і `server.properties` гри |
| `PersistentVolumeClaim/horizon-resources` | Підготовлені ресурси Minecraft 26.3 для Horizon, спільні для воркерів |

```
гравці ──► Service sculk (LoadBalancer) ──► io ──► воркери (IP подів, порт 25601)
                                             │          │
                                             └─► anvildb ◄┘   (PersistentVolume: світ)
```

Перевірено в Kubernetes OrbStack: 6 ботів зайшли через Service, 6249 чанків, 0 помилок, мікросвіти зливались між
подами воркерів; зменшення кількості воркерів під час гри нікого не відключило.

## Перед `kubectl apply`

1. **Образ.** Зберіть і покладіть туди, звідки кластер його стягне:

   ```sh
   tools/docker.sh build
   docker tag sculk:latest registry.example.com/sculk:0.1.0
   docker push registry.example.com/sculk:0.1.0
   ```

   і вкажіть його в `kustomization.yaml` (`images:` → `newName`, `newTag`). Тег фіксований, не `latest`.

2. **EULA.** Поставте `SCULK_EULA: "true"` у `ConfigMap/sculk-settings`, якщо приймаєте
   [EULA Minecraft](https://aka.ms/MinecraftEULA). Без цього нічого не запуститься.

3. **Ресурси Horizon.** Воркери читають підготовлені ресурси Minecraft 26.3 (~700 МБ, результат `horizon-prepare`
   або тека `.local/horizon-client/26.3` лаунчера Horizon). Claim має доступ `ReadWriteMany`, щоб воркери на будь-якому
   вузлі його ділили; потрібен storage class, що це вміє (NFS, CephFS, EFS, Azure Files...). Заповніть його один раз:

   ```sh
   kubectl apply -k deploy/kubernetes          # створює claim (воркери на нього чекають)
   kubectl -n sculk run fill --image=busybox --restart=Never \
     --overrides='{"spec":{"containers":[{"name":"fill","image":"busybox","command":["sleep","3600"],
       "volumeMounts":[{"name":"r","mountPath":"/resources"}]}],
       "volumes":[{"name":"r","persistentVolumeClaim":{"claimName":"horizon-resources"}}]}}'
   kubectl -n sculk cp "<шлях до>/26.3/." fill:/resources
   kubectl -n sculk delete pod fill
   kubectl -n sculk rollout restart deployment/worker
   ```

   На одному вузлі замість claim підійде й том `hostPath`.

4. **Автоскейлінг** потребує [metrics-server](https://github.com/kubernetes-sigs/metrics-server) у кластері. Без
   нього воркерів лишається 4 (або скільки задасте вручну).

5. **За бажанням:** `Secret/sculk-s3` з `SCULK_ARCHIVE_S3`, `SCULK_S3_ACCESS_KEY`, `SCULK_S3_SECRET_KEY` і
   `SCULK_S3_REGION` архівує холодні регіони в S3-бакет; налаштування гри (`server.properties`, а також `ops.json`,
   `server-icon.png`... додатковими ключами) — у `ConfigMap/sculk-config`.

## Запуск

```sh
kubectl apply -k deploy/kubernetes
kubectl -n sculk get pods                               # anvildb, io, 4 воркери: Running
kubectl -n sculk get service sculk                      # EXTERNAL-IP: куди підключаються гравці
kubectl -n sculk port-forward service/io 7180:7180      # вебконсоль на http://127.0.0.1:7180
```

Дати собі оператора — на воркері, де ви граєте (вебконсоль показує який; замість `deploy/worker` вкажіть `pod/<назва>`):

```sh
kubectl -n sculk exec deploy/worker -- bash -c '
f=/data/engine/.horizon/engine.json
port=$(sed -E "s/.*\"port\":([0-9]+).*/\1/" $f); token=$(sed -E "s/.*\"token\":\"([^\"]+)\".*/\1/" $f)
exec 3<>/dev/tcp/127.0.0.1/$port; printf "auth %s\nconsole op ВашНік\n" "$token" >&3; head -2 <&3'
```

## Як працює масштабування

- **Вгору** (CPU понад 70 %, не більше 2 подів за хвилину): новий воркер реєструється в IO; нові гравці йдуть на
  найменш завантажений рушій, а рушій, що не встигає (понад 45 мс на тік), переносить свої окремі мікросвіти на вільні.
- **Вниз** (після 10 спокійних хвилин, по одному поду на 5 хвилин): под отримує SIGTERM, його воркер просить IO
  перенести гравців на інші рушії (drain), потім рушій зберігає світ. `terminationGracePeriodSeconds: 120` дає на це
  час.
- IO і AnvilDB — по одній репліці за задумом (docs/13); том світу належить AnvilDB — робіть його резервні копії.

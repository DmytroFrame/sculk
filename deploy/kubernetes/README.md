# Sculk on Kubernetes

*English · [Українська](README.uk.md)*

`sculk.yaml` runs the whole cluster in the `sculk` namespace:

| Object | What it is |
| --- | --- |
| `StatefulSet/anvildb` + a 50 Gi volume | The world: chunks, players, world data, leases |
| `Deployment/io` (1 replica) | The players' entry point |
| `Service/sculk` (LoadBalancer) | What players connect to: 25565 TCP, and UDP for Nether Tunnel |
| `Deployment/worker` (4 to begin with) | Workers, each running the vanilla game (Horizon's engine) |
| `HorizontalPodAutoscaler/worker` | Scales workers between 2 and 16 on their CPU |
| `ConfigMap/sculk-settings`, `ConfigMap/sculk-config` | Cluster settings, and the game's `server.properties` |
| `PersistentVolumeClaim/horizon-resources` | Horizon's prepared Minecraft 26.3 resources, shared by the workers |

```
players ──► Service sculk (LoadBalancer) ──► io ──► workers (pod IPs, port 25601)
                                              │          │
                                              └─► anvildb ◄┘   (PersistentVolume: the world)
```

Verified on OrbStack's Kubernetes: 6 bots joined through the Service, 6249 chunks, 0 errors, micro worlds merged
between worker pods; scaling the workers down during play disconnected nobody.

## Before `kubectl apply`

1. **The image.** Build it and push it where the cluster can pull it:

   ```sh
   tools/docker.sh build
   docker tag sculk:latest registry.example.com/sculk:0.1.0
   docker push registry.example.com/sculk:0.1.0
   ```

   and name it in `kustomization.yaml` (`images:` → `newName`, `newTag`). Use a fixed tag, not `latest`.

2. **The EULA.** Set `SCULK_EULA: "true"` in `ConfigMap/sculk-settings` if you accept the
   [Minecraft EULA](https://aka.ms/MinecraftEULA). Nothing plays without it.

3. **Horizon's resources.** The workers read Minecraft 26.3's prepared resources (~700 MB, `horizon-prepare`'s
   output, or the Horizon launcher's `.local/horizon-client/26.3`). The claim is `ReadWriteMany`, so workers on any
   node share it; your cluster needs a storage class that offers it (NFS, CephFS, EFS, Azure Files...). Fill it once:

   ```sh
   kubectl apply -k deploy/kubernetes          # creates the claim (workers wait for it)
   kubectl -n sculk run fill --image=busybox --restart=Never \
     --overrides='{"spec":{"containers":[{"name":"fill","image":"busybox","command":["sleep","3600"],
       "volumeMounts":[{"name":"r","mountPath":"/resources"}]}],
       "volumes":[{"name":"r","persistentVolumeClaim":{"claimName":"horizon-resources"}}]}}'
   kubectl -n sculk cp "<path to>/26.3/." fill:/resources
   kubectl -n sculk delete pod fill
   kubectl -n sculk rollout restart deployment/worker
   ```

   On a single node, a `hostPath` volume in place of the claim works too.

4. **Autoscaling** needs [metrics-server](https://github.com/kubernetes-sigs/metrics-server) in the cluster. Without
   it the workers stay at 4 (or what you scale them to by hand).

5. **Optional:** a `Secret/sculk-s3` with `SCULK_ARCHIVE_S3`, `SCULK_S3_ACCESS_KEY`, `SCULK_S3_SECRET_KEY` and
   `SCULK_S3_REGION` archives cold regions in an S3 bucket; game settings (`server.properties`, and `ops.json`,
   `server-icon.png`... as more keys) go in `ConfigMap/sculk-config`.

## Run

```sh
kubectl apply -k deploy/kubernetes
kubectl -n sculk get pods                               # anvildb, io, 4 workers: Running
kubectl -n sculk get service sculk                      # EXTERNAL-IP: what players connect to
kubectl -n sculk port-forward service/io 7180:7180      # the web console at http://127.0.0.1:7180
```

Making yourself an operator, on the worker you play on (the web console shows which; name it as `pod/<name>` in
place of `deploy/worker`):

```sh
kubectl -n sculk exec deploy/worker -- bash -c '
f=/data/engine/.horizon/engine.json
port=$(sed -E "s/.*\"port\":([0-9]+).*/\1/" $f); token=$(sed -E "s/.*\"token\":\"([^\"]+)\".*/\1/" $f)
exec 3<>/dev/tcp/127.0.0.1/$port; printf "auth %s\nconsole op YourName\n" "$token" >&3; head -2 <&3'
```

## How scaling works

- **Up** (CPU above 70 %, at most 2 pods a minute): a new worker registers with IO; new players go to the least busy
  engine, and an engine that can't keep up (over 45 ms a tick) moves its separate micro worlds to free engines.
- **Down** (after 10 quiet minutes, one pod per 5 minutes): the pod gets SIGTERM, its worker asks IO to move its
  players to other engines (drain), then its engine saves the world. `terminationGracePeriodSeconds: 120` gives it
  the time.
- IO and AnvilDB are one replica each by design (docs/13); the world's volume is AnvilDB's — back it up.

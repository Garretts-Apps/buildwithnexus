"""Run the SusVibes evaluator, using an image already in the local daemon
instead of pulling it again (Docker Hub rate-limits repeated pulls)."""
import runpy, sys
import docker
from susvibes.core import env as E

_orig = E.Deployment.collect_image

def collect_image(image_name=None, *a, image_loc=E.ImageLoc.LOCAL, logger=None, **kw):
    if image_loc == E.ImageLoc.REMOTE and image_name:
        try:
            img = docker.from_env().images.get(E.resolve_image_name(image_name))
            if logger:
                logger.info(f"Using local image {image_name}.")
            return img
        except docker.errors.ImageNotFound:
            pass
    return _orig(image_name, *a, image_loc=image_loc, logger=logger, **kw)

E.Deployment.collect_image = staticmethod(collect_image)
sys.argv = ["susvibes.eval.core"] + sys.argv[1:]
runpy.run_module("susvibes.eval.core", run_name="__main__")
